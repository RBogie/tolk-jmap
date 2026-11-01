//! Minimal JMAP client (RFC 8620, RFC 8621): session discovery and method calls.
//!
//! Every request carries the device's IP in `X-Forwarded-For`, so the JMAP
//! server's rate limiting and fail2ban act on the phone instead of on us.

use crate::LockExt;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    auth::{Credentials, WithCredentials},
    config::Config,
};

mod groupware;
mod mail;
mod people;

pub use groupware::{AddressBook, Calendar};
pub use mail::{BodyPart, BodyValue, BodyValues, Email, EmailAddress, EmailMeta, Mailbox};
pub use people::Person;

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
const CALENDARS_PARSE: &str = "urn:ietf:params:jmap:calendars:parse";
const PRINCIPALS: &str = "urn:ietf:params:jmap:principals";
const VACATION: &str = "urn:ietf:params:jmap:vacationresponse";

/// A JMAP data type Tolk syncs, with the capability that serves it.
#[derive(Clone, Copy, Debug)]
pub struct DataType {
    pub name: &'static str,
    capability: &'static str,
}

pub const EMAIL: DataType = DataType {
    name: "Email",
    capability: MAIL,
};
pub const CONTACT_CARD: DataType = DataType {
    name: "ContactCard",
    capability: CONTACTS,
};
pub const MAILBOX: DataType = DataType {
    name: "Mailbox",
    capability: MAIL,
};
pub const ADDRESS_BOOK: DataType = DataType {
    name: "AddressBook",
    capability: CONTACTS,
};
pub const CALENDAR: DataType = DataType {
    name: "Calendar",
    capability: CALENDARS,
};
pub const CALENDAR_EVENT: DataType = DataType {
    name: "CalendarEvent",
    capability: CALENDARS,
};

/// How long the people collected for recipient lookup are reused.
const PEOPLE_TTL: Duration = Duration::from_secs(15 * 60);

/// Per user: when the known people were collected, and the list.
/// Values per key that expire after a while; at most 10,000 keys.
struct TtlCache<V> {
    ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, V)>>,
}

impl<V: Clone> TtlCache<V> {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::default(),
        }
    }

    fn get(&self, key: &str) -> Option<V> {
        self.entries
            .locked()
            .get(key)
            .filter(|(at, _)| at.elapsed() < self.ttl)
            .map(|(_, value)| value.clone())
    }

    fn insert(&self, key: String, value: V) {
        let mut entries = self.entries.locked();
        if entries.len() >= 10_000 {
            entries.clear();
        }
        entries.insert(key, (Instant::now(), value));
    }

    fn prune(&self) {
        self.entries
            .locked()
            .retain(|_, (at, _)| at.elapsed() < self.ttl);
    }
}

/// What a pool's clients remember between requests, per account.
struct Caches {
    /// Contacts and correspondents, for recipient lookup (by user name).
    people: TtlCache<Arc<Vec<Person>>>,
    /// The account's own addresses (by account id).
    identities: TtlCache<Vec<String>>,
}

impl Default for Caches {
    fn default() -> Self {
        Self {
            people: TtlCache::new(PEOPLE_TTL),
            identities: TtlCache::new(PEOPLE_TTL),
        }
    }
}

/// Whether an error chain contains an HTTP 401 from the JMAP server, which
/// means the cached session's credentials stopped working.
pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|e| e.status() == Some(reqwest::StatusCode::UNAUTHORIZED))
}

/// A JMAP method error (RFC 8620 section 3.6.2), such as `forbidden`.
#[derive(Debug)]
pub struct MethodError {
    pub method: String,
    /// The error object; its `type` says what went wrong.
    pub error: Value,
}

impl std::fmt::Display for MethodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed: {}", self.method, self.error)
    }
}

impl std::error::Error for MethodError {}

/// Whether an error says the account does not offer the data at all (a
/// shared account that advertises a capability it does not serve).
pub fn is_not_offered(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|cause| cause.downcast_ref::<MethodError>())
        .any(|e| {
            [
                "accountNotSupportedByMethod",
                "accountNotFound",
                "forbidden",
            ]
            .iter()
            .any(|t| e.error["type"] == *t)
        })
}

#[derive(Debug)]
pub enum AuthError {
    Invalid,
    Backend(anyhow::Error),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    pub event_source_url: String,
    #[serde(default)]
    pub capabilities: HashMap<String, Value>,
    /// Every account the user may access: their own and those shared with
    /// them.
    #[serde(default)]
    pub accounts: HashMap<String, Account>,
    pub primary_accounts: HashMap<String, String>,
    pub username: String,
}

impl Session {
    /// Makes the session's URLs absolute: Stalwart 1.0 can send them relative
    /// to the server (`/jmap/`).
    fn resolve_urls(&mut self, base: &reqwest::Url) {
        let origin = base.origin().ascii_serialization();
        for url in [
            &mut self.api_url,
            &mut self.download_url,
            &mut self.upload_url,
            &mut self.event_source_url,
        ] {
            if url.starts_with('/') {
                *url = format!("{origin}{url}");
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_read_only: bool,
    #[serde(default)]
    pub account_capabilities: HashMap<String, Value>,
}

/// User name and a keyed hash of the password.
type SessionKey = (String, u64);

/// One JMAP server Tolk talks to.
struct Server {
    session_url: String,
    /// The server's base URL.
    base_url: String,
    /// What device state of its accounts is stored under besides the account
    /// id: `None` for `JMAP_URL`, the host for a server from `JMAP_SERVERS`
    /// (account ids of different servers can be the same).
    scope: Option<Arc<str>>,
    caches: Arc<Caches>,
}

impl Server {
    fn new(base_url: &str, scope: Option<Arc<str>>) -> Arc<Self> {
        Arc::new(Self {
            session_url: format!("{base_url}/.well-known/jmap"),
            base_url: base_url.to_owned(),
            scope,
            caches: Arc::default(),
        })
    }
}

/// Caches sessions per credentials; a cache hit also skips re-verifying them.
pub struct JmapPool {
    http: reqwest::Client,
    /// `JMAP_URL`: logins without a mapped mail domain.
    default: Arc<Server>,
    /// `JMAP_SERVERS`: by mail domain (lowercase).
    by_domain: Vec<(String, Arc<Server>)>,
    ttl: Duration,
    /// Keyed by user and a keyed hash of the password, so the cache does not
    /// hold passwords as keys and a changed password misses it.
    sessions: Mutex<HashMap<SessionKey, (Instant, Arc<Session>)>>,
    key_hasher: std::hash::RandomState,
}

impl JmapPool {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let default = Server::new(&config.jmap_url, None);
        let mut servers: Vec<Arc<Server>> = Vec::new();
        let by_domain = config
            .jmap_servers
            .iter()
            .map(|(domain, url)| {
                let server = if *url == config.jmap_url {
                    default.clone()
                } else if let Some(known) = servers.iter().find(|s| s.base_url == *url) {
                    known.clone()
                } else {
                    let server = Server::new(url, Some(host_of(url).into()));
                    servers.push(server.clone());
                    server
                };
                (domain.to_ascii_lowercase(), server)
            })
            .collect();
        Ok(Self {
            http: crate::http_client()
                .timeout(Duration::from_secs(30))
                .build()?,
            default,
            by_domain,
            ttl: config.auth_cache_ttl,
            sessions: Mutex::new(HashMap::new()),
            key_hasher: std::hash::RandomState::new(),
        })
    }

    /// The server for a login name or address: by its mail domain, else
    /// `JMAP_URL`.
    fn server_for(&self, user: &str) -> &Arc<Server> {
        user.rsplit_once('@')
            .and_then(|(_, domain)| {
                self.by_domain
                    .iter()
                    .find(|(d, _)| d.eq_ignore_ascii_case(domain))
            })
            .map_or(&self.default, |(_, server)| server)
    }

    /// Verifies the credentials and returns a client acting for `client_ip`.
    pub async fn connect(
        &self,
        creds: &Credentials,
        client_ip: Option<IpAddr>,
    ) -> Result<JmapClient, AuthError> {
        let server = self.server_for(&creds.username);
        let session = match self.cached(creds) {
            Some(session) => session,
            None => {
                let session = Arc::new(self.fetch_session(server, creds, client_ip).await?);
                self.sessions
                    .locked()
                    .insert(self.cache_key(creds), (Instant::now(), session.clone()));
                session
            }
        };
        Ok(JmapClient {
            http: self.http.clone(),
            session,
            creds: creds.clone(),
            client_ip,
            caches: server.caches.clone(),
            scope: server.scope.clone(),
            account: None,
        })
    }

    /// Whether the `JMAP_URL` server answers at all (readiness check). Any
    /// HTTP response counts, including the 401 an anonymous request gets.
    /// Servers from `JMAP_SERVERS` are left out: one of them being down
    /// should not take Tolk away from everyone else.
    pub async fn reachable(&self) -> anyhow::Result<()> {
        self.http
            .get(&self.default.session_url)
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        Ok(())
    }

    /// Drops expired sessions and people lists (maintenance).
    pub fn prune(&self) {
        self.sessions
            .locked()
            .retain(|_, (at, _)| at.elapsed() < self.ttl);
        for server in std::iter::once(&self.default).chain(self.by_domain.iter().map(|(_, s)| s)) {
            server.caches.people.prune();
            server.caches.identities.prune();
        }
    }

    /// Passes an Autodiscover request we do not answer (Outlook desktop's
    /// IMAP and SMTP settings) on to the JMAP server of `address`, which may.
    pub async fn forward_autodiscover(
        &self,
        address: &str,
        authorization: Option<&str>,
        content_type: Option<&str>,
        client_ip: IpAddr,
        body: Vec<u8>,
    ) -> anyhow::Result<(u16, Option<String>, Vec<u8>)> {
        let mut request = forwarded_for(
            self.http.post(format!(
                "{}/autodiscover/autodiscover.xml",
                self.server_for(address).base_url
            )),
            Some(client_ip),
        )
        .timeout(Duration::from_secs(10))
        .body(body);
        if let Some(authorization) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok((status, content_type, response.bytes().await?.to_vec()))
    }

    /// Forgets a cached session, so the next request verifies the credentials again.
    pub fn evict(&self, creds: &Credentials) {
        self.sessions.locked().remove(&self.cache_key(creds));
    }

    fn cache_key(&self, creds: &Credentials) -> SessionKey {
        use std::hash::BuildHasher;
        (
            creds.username.clone(),
            self.key_hasher.hash_one(&creds.password),
        )
    }

    fn cached(&self, creds: &Credentials) -> Option<Arc<Session>> {
        let mut sessions = self.sessions.locked();
        sessions.retain(|_, (at, _)| at.elapsed() < self.ttl);
        sessions
            .get(&self.cache_key(creds))
            .map(|(_, session)| session.clone())
    }

    async fn fetch_session(
        &self,
        server: &Server,
        creds: &Credentials,
        client_ip: Option<IpAddr>,
    ) -> Result<Session, AuthError> {
        let response = forwarded_for(self.http.get(&server.session_url), client_ip)
            .credentials(creds)
            .send()
            .await
            .map_err(|e| AuthError::Backend(e.into()))?;
        match response.status() {
            s if s.is_success() => {
                let base = response.url().clone();
                let mut session: Session = response.json().await.map_err(|e| {
                    AuthError::Backend(anyhow::Error::from(e).context("invalid JMAP session"))
                })?;
                session.resolve_urls(&base);
                let mut capabilities: Vec<&str> =
                    session.capabilities.keys().map(String::as_str).collect();
                capabilities.sort_unstable();
                tracing::info!(user = %creds.username, ?capabilities, "JMAP session");
                Ok(session)
            }
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                Err(AuthError::Invalid)
            }
            s => Err(AuthError::Backend(anyhow!(
                "JMAP session request returned {s}"
            ))),
        }
    }
}

/// `https://mail.example.org:8443/x` as `mail.example.org:8443`.
fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| match u.port() {
            Some(port) => Some(format!("{}:{port}", u.host_str()?)),
            None => u.host_str().map(str::to_owned),
        })
        .unwrap_or_else(|| url.to_owned())
}

fn forwarded_for(request: reqwest::RequestBuilder, ip: Option<IpAddr>) -> reqwest::RequestBuilder {
    match ip {
        Some(ip) => request.header("X-Forwarded-For", ip.to_string()),
        None => request,
    }
}

pub enum Changes {
    Changes {
        new_state: String,
        has_more: bool,
        created: Vec<String>,
        updated: Vec<String>,
        destroyed: Vec<String>,
    },
    /// The state is too old; the caller has to start over.
    CannotCalculate,
}

/// What a `<type>/set` call should change.
#[derive(Debug, Default)]
pub struct SetRequest {
    create: Map<String, Value>,
    update: Map<String, Value>,
    destroy: Vec<String>,
    /// Type-specific arguments (`sendSchedulingMessages`, ...).
    extra: Map<String, Value>,
}

impl SetRequest {
    /// Objects to create, by creation id.
    pub fn create(mut self, create: Map<String, Value>) -> Self {
        self.create = create;
        self
    }

    /// Patches, by object id.
    pub fn update(mut self, update: Map<String, Value>) -> Self {
        self.update = update;
        self
    }

    pub fn destroy(mut self, ids: Vec<String>) -> Self {
        self.destroy = ids;
        self
    }

    /// A type-specific argument.
    pub fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(name.to_owned(), value.into());
        self
    }

    fn is_empty(&self) -> bool {
        self.create.is_empty() && self.update.is_empty() && self.destroy.is_empty()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetResult {
    /// Creation id to the created object (with its server `id`).
    #[serde(default, deserialize_with = "null_as_default")]
    pub created: HashMap<String, Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub not_created: HashMap<String, Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub not_updated: HashMap<String, Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub not_destroyed: HashMap<String, Value>,
}

impl SetResult {
    /// The JMAP error type when the update or destroy of `id` failed.
    pub fn error(&self, id: &str) -> Option<&str> {
        self.not_updated
            .get(id)
            .or_else(|| self.not_destroyed.get(id))
            .or_else(|| self.not_created.get(id))
            .map(|e| e["type"].as_str().unwrap_or("serverFail"))
    }
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone)]
pub struct JmapClient {
    http: reqwest::Client,
    session: Arc<Session>,
    creds: Credentials,
    client_ip: Option<IpAddr>,
    caches: Arc<Caches>,
    /// See [`Server::scope`].
    scope: Option<Arc<str>>,
    /// Another account (shared with the user) that contacts and calendar
    /// calls go to, instead of the primary ones (see [`Self::in_account`]).
    account: Option<Arc<str>>,
}

impl JmapClient {
    pub fn username(&self) -> &str {
        &self.session.username
    }

    /// The JMAP mail account.
    pub fn account_id(&self) -> anyhow::Result<&str> {
        self.primary(MAIL)
    }

    /// What device state is stored under: the mail account, so every login
    /// form of one account (`rob`, `rob@example.com`) shares it, prefixed
    /// with the server's host for a server from `JMAP_SERVERS`.
    pub fn user_key(&self) -> anyhow::Result<std::borrow::Cow<'_, str>> {
        let account = self.account_id()?;
        Ok(match &self.scope {
            None => account.into(),
            Some(scope) => format!("{scope}/{account}").into(),
        })
    }

    /// The account mail calls go to: the shared one, else the primary.
    pub fn account_for_mail(&self) -> anyhow::Result<&str> {
        self.account_for(MAIL)
    }

    /// This client for another account the user may access (calendars and
    /// address books shared with them); mail stays with the primary one.
    pub fn in_account(&self, account: &str) -> Self {
        Self {
            account: Some(account.into()),
            ..self.clone()
        }
    }

    /// Whether this client works in a shared account (see [`Self::in_account`]).
    pub fn is_shared(&self) -> bool {
        self.account.is_some()
    }

    /// The shared account this client works in, if any.
    pub fn shared_account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    /// Accounts other than the primary one that offer `capability`, with
    /// their names, by id.
    pub(crate) fn shared_accounts(&self, capability: &str) -> Vec<(&str, &str)> {
        let primary = self.session.primary_accounts.get(capability);
        let mut accounts: Vec<(&str, &str)> = self
            .session
            .accounts
            .iter()
            .filter(|(id, a)| {
                Some(*id) != primary && a.account_capabilities.contains_key(capability)
            })
            .map(|(id, a)| (id.as_str(), a.name.as_str()))
            .collect();
        accounts.sort_unstable();
        accounts
    }

    /// Whether the account this client works in may not be changed.
    pub fn is_read_only(&self, capability: &str) -> bool {
        self.account_for(capability)
            .ok()
            .and_then(|id| self.session.accounts.get(id))
            .is_some_and(|a| a.is_read_only)
    }

    /// The account for a capability: the one chosen with
    /// [`Self::in_account`], else the primary one.
    pub(crate) fn account_for(&self, capability: &str) -> anyhow::Result<&str> {
        match &self.account {
            Some(account) => Ok(account),
            None => self.primary(capability),
        }
    }

    fn primary(&self, capability: &str) -> anyhow::Result<&str> {
        self.session
            .primary_accounts
            .get(capability)
            .map(String::as_str)
            .with_context(|| format!("account has no {capability} capability"))
    }

    /// The current state string of a data type.
    pub async fn state(&self, data_type: DataType) -> anyhow::Result<String> {
        let method = format!("{}/get", data_type.name);
        let account = self.account_for(data_type.capability)?;
        let args = self
            .call_with(
                data_type.capability,
                &method,
                json!({ "accountId": account, "ids": [], "properties": ["id"] }),
            )
            .await?;
        args["state"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("{method} without state"))
    }

    /// Creates, updates and destroys objects; failures come back per id in
    /// the result. An empty request makes no call.
    pub async fn set(&self, data_type: DataType, request: SetRequest) -> anyhow::Result<SetResult> {
        if request.is_empty() {
            return Ok(SetResult::default());
        }
        let method = format!("{}/set", data_type.name);
        let mut args = request.extra;
        args.insert(
            "accountId".to_owned(),
            self.account_for(data_type.capability)?.into(),
        );
        args.insert("create".to_owned(), Value::Object(request.create));
        args.insert("update".to_owned(), Value::Object(request.update));
        args.insert("destroy".to_owned(), json!(request.destroy));
        let args = Value::Object(args);
        let response = self
            .call_with(data_type.capability, &method, args.clone())
            .await?;
        tracing::debug!(request = %args, %response, "{method}");
        Ok(serde_json::from_value(response)?)
    }

    /// Full objects (all properties), in batches the server accepts.
    pub async fn get_objects(
        &self,
        data_type: DataType,
        ids: &[&str],
    ) -> anyhow::Result<Vec<Value>> {
        let method = format!("{}/get", data_type.name);
        let account = self.account_for(data_type.capability)?;
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(self.max_objects_in_get()) {
            let mut args = self
                .call_with(
                    data_type.capability,
                    &method,
                    json!({ "accountId": account, "ids": batch }),
                )
                .await?;
            out.extend(serde_json::from_value::<Vec<Value>>(args["list"].take())?);
        }
        Ok(out)
    }

    /// Every object of a type (mailboxes, address books, calendars) with
    /// these properties.
    pub async fn get_all<T: serde::de::DeserializeOwned>(
        &self,
        data_type: DataType,
        properties: &[&str],
    ) -> anyhow::Result<Vec<T>> {
        let mut args = self
            .call_with(
                data_type.capability,
                &format!("{}/get", data_type.name),
                json!({
                    "accountId": self.account_for(data_type.capability)?,
                    "ids": null,
                    "properties": properties,
                }),
            )
            .await?;
        Ok(serde_json::from_value(args["list"].take())?)
    }

    /// Server limit for ids per `/get` call, capped to keep requests modest.
    fn max_objects_in_get(&self) -> usize {
        self.session
            .capabilities
            .get(CORE)
            .and_then(|core| core["maxObjectsInGet"].as_u64())
            .map_or(500, |n| n as usize)
            .clamp(1, 500)
    }

    /// Runs method calls in one request and returns each response's name and
    /// arguments; a method-level error comes back as name `"error"`.
    async fn call_many(&self, calls: Vec<(&str, Value)>) -> anyhow::Result<Vec<(String, Value)>> {
        self.request(&[CORE, MAIL, SUBMISSION], calls).await
    }

    async fn request(
        &self,
        using: &[&str],
        calls: Vec<(&str, Value)>,
    ) -> anyhow::Result<Vec<(String, Value)>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            method_responses: Vec<(String, Value, String)>,
        }

        let methods: Vec<&str> = calls.iter().map(|(m, _)| *m).collect();
        let method_calls: Vec<Value> = calls
            .into_iter()
            .enumerate()
            .map(|(i, (method, args))| json!([method, args, i.to_string()]))
            .collect();
        let body = json!({ "using": using, "methodCalls": method_calls });
        let response: Response =
            forwarded_for(self.http.post(&self.session.api_url), self.client_ip)
                .credentials(&self.creds)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
                .with_context(|| format!("invalid {} response", methods.join("+")))?;
        Ok(response
            .method_responses
            .into_iter()
            .map(|(name, args, _)| (name, args))
            .collect())
    }

    /// Runs a single method call and returns its response arguments.
    async fn call(&self, method: &str, args: Value) -> anyhow::Result<Value> {
        self.call_using(&[CORE, MAIL, SUBMISSION], method, args)
            .await
    }

    /// Runs a single method call of a capability (besides core) and
    /// returns its response arguments; a method error is an error.
    async fn call_with(
        &self,
        capability: &str,
        method: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        self.call_using(&[CORE, capability], method, args).await
    }

    /// Runs a single method call with these capabilities and returns its
    /// response arguments; a method error is an error.
    async fn call_using(&self, using: &[&str], method: &str, args: Value) -> anyhow::Result<Value> {
        let (name, args) = self
            .request(using, vec![(method, args)])
            .await?
            .into_iter()
            .next()
            .with_context(|| format!("empty {method} response"))?;
        if name == "error" {
            return Err(MethodError {
                method: method.to_owned(),
                error: args,
            }
            .into());
        }
        Ok(args)
    }

    /// One page of `<type>/changes` since `since_state`.
    pub async fn changes(
        &self,
        data_type: DataType,
        since_state: &str,
        max_changes: usize,
    ) -> anyhow::Result<Changes> {
        let (using, account_id) = (
            [CORE, data_type.capability],
            self.account_for(data_type.capability)?,
        );
        let data_type = data_type.name;
        let method = format!("{data_type}/changes");
        let (name, mut args) = self
            .request(
                &using,
                vec![(
                    method.as_str(),
                    json!({ "accountId": account_id, "sinceState": since_state, "maxChanges": max_changes }),
                )],
            )
            .await?
            .into_iter()
            .next()
            .with_context(|| format!("empty {method} response"))?;
        if name == "error" {
            if args["type"] == "cannotCalculateChanges" {
                return Ok(Changes::CannotCalculate);
            }
            return Err(MethodError {
                method,
                error: args,
            }
            .into());
        }
        let ids = |v: &mut Value| -> anyhow::Result<Vec<String>> {
            Ok(serde_json::from_value(v.take())?)
        };
        Ok(Changes::Changes {
            new_state: args["newState"]
                .as_str()
                .context("changes without newState")?
                .to_owned(),
            has_more: args["hasMoreChanges"].as_bool().unwrap_or(false),
            created: ids(&mut args["created"])?,
            updated: ids(&mut args["updated"])?,
            destroyed: ids(&mut args["destroyed"])?,
        })
    }

    /// Every id a `/query` matches, paged. A list that cannot be read
    /// completely is an error, never a shorter list: a missing id would turn
    /// into a delete on the device.
    async fn query_all(
        &self,
        using: &[&str],
        account_id: &str,
        method: &str,
        filter: Value,
        sort: Option<Value>,
    ) -> anyhow::Result<Vec<String>> {
        /// Paging keeps each response small; servers cap the page size too.
        const PAGE: usize = 1000;
        const MAX_ITEMS: usize = 250_000;
        let mut ids: Vec<String> = Vec::new();
        loop {
            let mut args = json!({
                "accountId": account_id,
                "filter": filter,
                "position": ids.len(),
                "limit": PAGE,
                "calculateTotal": true,
            });
            if let Some(sort) = &sort {
                args["sort"] = sort.clone();
            }
            let mut result = self.call_using(using, method, args).await?;
            let page: Vec<String> = serde_json::from_value(result["ids"].take())?;
            let total = result["total"].as_u64().context("query without total")? as usize;
            if total > MAX_ITEMS {
                bail!("{method} matches {total} items, more than the supported {MAX_ITEMS}");
            }
            let empty = page.is_empty();
            ids.extend(page);
            if ids.len() >= total {
                return Ok(ids);
            }
            if empty {
                bail!(
                    "{method} returned {} of {total} items (the result changed while paging)",
                    ids.len()
                );
            }
        }
    }

    /// Uploads a blob and returns its id.
    pub async fn upload(&self, data: Vec<u8>, mime_type: &str) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Uploaded {
            blob_id: String,
        }
        let url = self
            .session
            .upload_url
            .replace("{accountId}", &encode_component(self.account_for(MAIL)?));
        let uploaded: Uploaded = forwarded_for(self.http.post(url), self.client_ip)
            .credentials(&self.creds)
            .header(reqwest::header::CONTENT_TYPE, mime_type)
            .body(data)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(uploaded.blob_id)
    }

    /// Follows the account's EventSource push stream (RFC 8620 7.3) and calls
    /// `on_change` with the changed data types of every state change (`None`
    /// when they are unknown). Returns when the stream ends.
    pub async fn watch_changes(
        &self,
        mut on_change: impl FnMut(Option<Vec<String>>),
    ) -> anyhow::Result<()> {
        /// Reconnecting now and then guards against silently dead streams.
        const STREAM_LIFETIME: Duration = Duration::from_secs(30 * 60);
        let url = self
            .session
            .event_source_url
            .replace(
                "{types}",
                "Email,Mailbox,ContactCard,AddressBook,CalendarEvent,Calendar",
            )
            .replace("{closeafter}", "no")
            .replace("{ping}", "60");
        let mut response = forwarded_for(self.http.get(url), self.client_ip)
            .credentials(&self.creds)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .timeout(STREAM_LIFETIME)
            .send()
            .await?
            .error_for_status()?;
        let mut buffer = String::new();
        while let Some(chunk) = response.chunk().await? {
            buffer.push_str(&String::from_utf8_lossy(&chunk).replace('\r', ""));
            for types in take_state_events(&mut buffer) {
                on_change(types);
            }
        }
        Ok(())
    }

    pub async fn download(
        &self,
        blob_id: &str,
        name: &str,
        mime_type: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let bytes = self
            .download_stream(blob_id, name, mime_type)
            .await?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    /// A download whose body has not been read yet: the status is known
    /// (an error status is an error), the content can be streamed.
    pub async fn download_stream(
        &self,
        blob_id: &str,
        name: &str,
        mime_type: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.download_range(blob_id, name, mime_type, None).await
    }

    /// [`Self::download_stream`] asking for bytes `start..=end` only. Servers
    /// need not honour it: a 206 answer is the range, a 200 answer the
    /// whole blob.
    pub async fn download_range(
        &self,
        blob_id: &str,
        name: &str,
        mime_type: &str,
        range: Option<(u64, u64)>,
    ) -> anyhow::Result<reqwest::Response> {
        let url = self
            .session
            .download_url
            .replace("{accountId}", &encode_component(self.account_for(MAIL)?))
            .replace("{blobId}", &encode_component(blob_id))
            .replace("{name}", &encode_component(name))
            .replace("{type}", &encode_component(mime_type));
        let mut request = forwarded_for(self.http.get(url), self.client_ip);
        if let Some((start, end)) = range {
            request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
        }
        Ok(request
            .credentials(&self.creds)
            .send()
            .await?
            .error_for_status()?)
    }
}

#[cfg(test)]
impl JmapClient {
    /// A client for a server that does not exist, for code paths that must
    /// not reach JMAP.
    pub fn for_tests() -> Self {
        Self {
            http: crate::http_client().build().expect("HTTP client"),
            session: Arc::new(Session {
                api_url: "http://127.0.0.1:9/api".to_owned(),
                download_url: String::new(),
                upload_url: String::new(),
                event_source_url: String::new(),
                capabilities: HashMap::new(),
                accounts: HashMap::new(),
                primary_accounts: HashMap::new(),
                username: "rob@example.com".to_owned(),
            }),
            creds: Credentials {
                username: "rob".to_owned(),
                password: String::new(),
                bearer: false,
            },
            client_ip: None,
            caches: Arc::default(),
            scope: None,
            account: None,
        }
    }
}

/// Takes the complete events from `buffer` and returns, per `state` event,
/// the data types it reports changed in any account (`None` when the data
/// is unreadable). Shared accounts' changes count too: they wake the
/// folders of that kind, which each check their own account.
fn take_state_events(buffer: &mut String) -> Vec<Option<Vec<String>>> {
    let mut events = Vec::new();
    while let Some(end) = buffer.find("\n\n") {
        let event: String = buffer.drain(..end + 2).collect();
        let is_state = event
            .lines()
            .filter_map(|line| line.strip_prefix("event:"))
            .any(|name| name.trim() == "state");
        if !is_state {
            continue;
        }
        let data: String = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect();
        let types = serde_json::from_str::<Value>(&data).ok().and_then(|data| {
            let accounts = data["changed"].as_object()?;
            Some(
                accounts
                    .values()
                    .filter_map(Value::as_object)
                    .flat_map(|types| types.keys().cloned())
                    .collect(),
            )
        });
        events.push(types);
    }
    events
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn encode_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_session_urls() {
        let mut session: Session = serde_json::from_value(json!({
            "apiUrl": "/jmap/",
            "downloadUrl": "/jmap/download/{accountId}/{blobId}/{name}?accept={type}",
            "uploadUrl": "https://up.example.org/{accountId}/",
            "eventSourceUrl": "/jmap/eventsource/?types={types}",
            "primaryAccounts": {},
            "username": "a",
        }))
        .unwrap();
        session.resolve_urls(&"http://127.0.0.1:18080/jmap/session".parse().unwrap());
        assert_eq!(session.api_url, "http://127.0.0.1:18080/jmap/");
        assert_eq!(
            session.download_url,
            "http://127.0.0.1:18080/jmap/download/{accountId}/{blobId}/{name}?accept={type}"
        );
        assert_eq!(session.upload_url, "https://up.example.org/{accountId}/");
    }

    #[test]
    fn encodes_url_components() {
        assert_eq!(encode_component("message/rfc822"), "message%2Frfc822");
        assert_eq!(encode_component("a b.eml"), "a%20b.eml");
    }

    #[test]
    fn reads_state_events() {
        let mut buffer = String::from(
            "event: ping\ndata: {\"interval\":60}\n\n\
             event: state\ndata: {\"changed\":{\"a\":{\"Email\":\"s1\",\"Mailbox\":\"s2\"}}}\n\n\
             event: state\ndata: {\"chan",
        );
        assert_eq!(
            take_state_events(&mut buffer),
            vec![Some(vec!["Email".to_owned(), "Mailbox".to_owned()])]
        );
        assert_eq!(buffer, "event: state\ndata: {\"chan");
        buffer.push_str("ged\":{}}\n\nevent: state\ndata: garbage\n\n");
        assert_eq!(take_state_events(&mut buffer), vec![Some(vec![]), None]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn parses_set_failures() {
        let result: SetResult = serde_json::from_value(json!({
            "updated": { "a": null },
            "notUpdated": { "b": { "type": "notFound" } },
            "notDestroyed": null,
        }))
        .unwrap();
        assert_eq!(result.error("a"), None);
        assert_eq!(result.error("b"), Some("notFound"));
    }
}
