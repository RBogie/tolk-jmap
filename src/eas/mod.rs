//! The `/Microsoft-Server-ActiveSync` endpoint (MS-ASHTTP).

pub mod calendar;
pub mod compose;
pub mod contacts;
mod device_changes;
pub mod drafts;
pub mod email;
pub mod find;
pub mod folder_ops;
pub mod folder_sync;
pub mod item_operations;
pub mod jscalendar;
pub mod jscontact;
pub mod meeting_response;
pub mod move_items;
pub mod ping;
pub mod provision;
pub mod query;
pub mod resolve_recipients;
pub mod search;
pub mod settings;
pub mod stream;
pub mod sync;
pub mod tz;
pub mod v16;
pub mod view;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};

use crate::{
    AppState,
    auth::Credentials,
    jmap::{self, AuthError, JmapClient},
    limits,
    store::Store,
    traffic::Exchange,
    wbxml::{
        self, Element, Tag,
        cp::{
            airsync, compose_mail as cm, folder_hierarchy, item_estimate, item_operations as io,
            meeting_response as mresp, move_items as mv, search as sp,
        },
    },
};

pub const EAS_PATH: &str = "/Microsoft-Server-ActiveSync";

const PROTOCOL_COMMANDS: &str = "Sync,SendMail,SmartForward,SmartReply,FolderSync,FolderCreate,\
    FolderDelete,FolderUpdate,MoveItems,GetItemEstimate,Settings,Ping,ItemOperations,Provision,\
    Search,MeetingResponse,ResolveRecipients,Find";
const WBXML_CONTENT_TYPE: &str = "application/vnd.ms-sync.wbxml";

/// One decoded command, as seen by the command handlers.
pub struct Request<'a> {
    pub user: &'a str,
    pub device_id: &'a str,
    /// The account's primary address, reported to the device.
    pub email: &'a str,
    /// All the account's addresses (from its identities), comma separated.
    pub own: &'a str,
    /// From `X-MS-PolicyKey`, or from the base64 query form.
    pub policy_key: Option<u32>,
    /// The EAS version the device speaks, as 141, 160, 161.
    pub version: u16,
    pub body: Option<Element>,
    pub store: &'a Store,
    /// Whether refreshed collection views are saved. Ping only reads: it
    /// runs without the device lock, so saving would race with a Sync.
    pub persist_views: bool,
    pub caps: crate::config::Caps,
    /// Attachments streamed into the response (see [`stream`]).
    pub downloads: stream::Downloads,
}

impl Request<'_> {
    fn body_with_root(&self, root: Tag) -> Result<&Element, CommandError> {
        match &self.body {
            Some(body) if body.tag == root => Ok(body),
            Some(body) => Err(CommandError::bad(format!(
                "expected {root}, got {}",
                body.tag
            ))),
            None => Err(CommandError::bad(format!("empty {root} request"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_client_behind_trusted_proxies() {
        let trusted: Vec<crate::config::Cidr> = vec![
            "192.0.2.68".parse().unwrap(),
            "172.16.0.0/12".parse().unwrap(),
        ];
        let traefik: IpAddr = "192.0.2.68".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "6.6.6.6, 81.2.3.4, 172.17.0.2".parse().unwrap(),
        );
        headers.insert("x-real-ip", "9.9.9.9".parse().unwrap());
        // Right-most untrusted hop; the spoofable left part is ignored.
        assert_eq!(
            forwarded_client(&trusted, traefik, &headers),
            "81.2.3.4".parse::<IpAddr>().unwrap()
        );
        // An untrusted peer's headers are ignored.
        let stranger: IpAddr = "81.9.9.9".parse().unwrap();
        assert_eq!(forwarded_client(&trusted, stranger, &headers), stranger);
        // Without X-Forwarded-For, X-Real-Ip is used.
        headers.remove("x-forwarded-for");
        assert_eq!(
            forwarded_client(&trusted, traefik, &headers),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
    }
}

#[cfg(test)]
impl<'a> Request<'a> {
    pub fn for_tests(store: &'a Store, body: Option<Element>) -> Self {
        Self {
            user: "rob",
            device_id: "d1",
            email: "rob@example.com",
            own: "rob@example.com",
            policy_key: None,
            version: 141,
            body,
            store,
            persist_views: true,
            caps: Default::default(),
            downloads: Default::default(),
        }
    }
}

#[derive(Debug)]
pub enum CommandError {
    BadRequest(String),
    Backend(anyhow::Error),
}

impl CommandError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }
}

impl From<anyhow::Error> for CommandError {
    fn from(e: anyhow::Error) -> Self {
        Self::Backend(e)
    }
}

pub type CommandResult = Result<Element, CommandError>;

pub struct EasResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// The body; for a streamed response the document with placeholders,
    /// which is what traffic capture records.
    pub body: Bytes,
    /// The body to send instead, when attachments are streamed into it.
    streamed: Option<(Vec<wbxml::Chunk>, Vec<reqwest::Response>)>,
}

impl EasResponse {
    fn status(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            streamed: None,
        }
    }

    fn unauthorized() -> Self {
        let mut response = Self::status(StatusCode::UNAUTHORIZED);
        response.headers.insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"ActiveSync\""),
        );
        response
    }

    fn options(versions: &str) -> Self {
        let mut response = Self::status(StatusCode::OK);
        let h = &mut response.headers;
        h.insert(header::ALLOW, HeaderValue::from_static("OPTIONS,POST"));
        h.insert("Public", HeaderValue::from_static("OPTIONS,POST"));
        let newest = versions.rsplit(',').next().unwrap_or(versions);
        if let Ok(value) = HeaderValue::from_str(newest) {
            h.insert("MS-Server-ActiveSync", value);
        }
        if let Ok(value) = HeaderValue::from_str(versions) {
            h.insert("MS-ASProtocolVersions", value);
        }
        h.insert(
            "MS-ASProtocolCommands",
            HeaderValue::from_static(PROTOCOL_COMMANDS),
        );
        response
    }

    fn wbxml(root: &Element) -> Self {
        let mut response = Self::status(StatusCode::OK);
        response.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(WBXML_CONTENT_TYPE),
        );
        response.body = wbxml::encode(root).into();
        response
    }

    fn streamed(root: &Element, downloads: Vec<reqwest::Response>) -> Self {
        let mut response = Self::wbxml(root);
        let chunks = wbxml::encode_parts(root);
        if let Some(length) = stream::length(&chunks, &downloads) {
            response
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
        response.streamed = Some((chunks, downloads));
        response
    }
}

impl IntoResponse for EasResponse {
    fn into_response(self) -> Response {
        match self.streamed {
            Some((chunks, downloads)) => {
                let body = axum::body::Body::from_stream(stream::body(chunks, downloads));
                (self.status, self.headers, body).into_response()
            }
            None => (self.status, self.headers, self.body).into_response(),
        }
    }
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    if !uri.path().eq_ignore_ascii_case(EAS_PATH) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let query = query::parse(uri.query());
    let creds = Credentials::from_headers(&headers);
    let client_ip = client_ip(&state, peer.ip(), &headers);
    // Every log line of the request carries these fields.
    static REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let started = std::time::Instant::now();
    let span = tracing::info_span!(
        "request",
        id = REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        user = creds.as_ref().map(|c| c.username.as_str()).unwrap_or("-"),
        device = query.device_id.as_deref().unwrap_or("-"),
        cmd = query.cmd.as_deref().unwrap_or(method.as_str()),
    );
    let (response, body) = tracing::Instrument::instrument(
        respond(
            &state,
            &method,
            &query,
            &headers,
            creds.as_ref(),
            client_ip,
            body,
        ),
        span,
    )
    .await;

    state.metrics.record_request(
        query.cmd.as_deref().unwrap_or(method.as_str()),
        response.status.as_u16(),
        started.elapsed(),
    );
    state
        .traffic
        .record(&Exchange {
            method: &method,
            uri: &uri,
            query: &query,
            user: creds.as_ref().map(|c| c.username.as_str()),
            request_headers: &headers,
            request_body: &body,
            response: &response,
        })
        .await;

    response.into_response()
}

/// The device's address. Behind a trusted proxy: the right-most address in
/// `X-Forwarded-For` that is not itself a trusted proxy, else `X-Real-Ip`.
/// Otherwise the peer address (headers from untrusted peers are ignored).
pub(crate) fn client_ip(state: &AppState, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    forwarded_client(&state.trusted_proxies, peer, headers)
}

fn forwarded_client(trusted: &[crate::config::Cidr], peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|range| range.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(forwarded) = header("x-forwarded-for") {
        for hop in forwarded.rsplit(',') {
            match hop.trim().parse::<IpAddr>() {
                Ok(ip) if !is_trusted(ip) => return ip,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }
    header("x-real-ip")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer)
}

async fn respond(
    state: &AppState,
    method: &Method,
    query: &query::EasQuery,
    headers: &HeaderMap,
    creds: Option<&Credentials>,
    client_ip: IpAddr,
    body: axum::body::Body,
) -> (EasResponse, Bytes) {
    // The body is only read once the login is known to be good, so nobody
    // can make the server buffer large uploads without an account.
    let refuse = |response: EasResponse| (response, Bytes::new());
    let Some(creds) = creds else {
        return refuse(EasResponse::unauthorized());
    };
    if state.logins.is_blocked(creds, client_ip) {
        tracing::warn!(user = %creds.username, %client_ip, "too many failed logins, refusing");
        return refuse(EasResponse::unauthorized());
    }
    let jmap = match state.jmap.connect(creds, Some(client_ip)).await {
        Ok(jmap) => {
            state.logins.record_success(creds, client_ip);
            jmap
        }
        Err(AuthError::Invalid) => {
            tracing::info!(user = %creds.username, %client_ip, "rejected credentials");
            state.logins.record_failure(creds, client_ip);
            state.metrics.record_login_failure();
            return refuse(EasResponse::unauthorized());
        }
        Err(AuthError::Backend(e)) => {
            tracing::warn!(error = format!("{e:#}"), "could not verify credentials");
            return refuse(EasResponse::status(StatusCode::SERVICE_UNAVAILABLE));
        }
    };

    if *method == Method::OPTIONS {
        return refuse(EasResponse::options(&state.eas_versions));
    }
    if *method != Method::POST {
        return refuse(EasResponse::status(StatusCode::METHOD_NOT_ALLOWED));
    }

    let cmd = query.cmd.as_deref().unwrap_or("?");
    let limit = limits::body_limit(cmd, state.max_request_bytes);
    let Ok(body) = axum::body::to_bytes(body, limit).await else {
        tracing::warn!(user = %creds.username, cmd, limit, "request body too large");
        return refuse(EasResponse::status(StatusCode::PAYLOAD_TOO_LARGE));
    };
    let response = run(state, query, headers, creds, &jmap, cmd, &body).await;
    (response, body)
}

/// Runs an authenticated command within the concurrency and time limits.
async fn run(
    state: &AppState,
    query: &query::EasQuery,
    headers: &HeaderMap,
    creds: &Credentials,
    jmap: &JmapClient,
    cmd: &str,
    body: &[u8],
) -> EasResponse {
    let busy = || {
        tracing::warn!(user = %creds.username, cmd, "too many requests at once");
        let mut response = EasResponse::status(StatusCode::SERVICE_UNAVAILABLE);
        response
            .headers
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
        response
    };
    // Decoding counts as command work; a request that turns out to wait for
    // changes then moves to a long-poll slot, so waiting requests cannot use
    // up the slots everything else needs.
    let Ok(mut permit) = state.concurrency.commands.try_acquire() else {
        return busy();
    };
    let body = match body {
        [] => None,
        bytes => match wbxml::decode(bytes) {
            Ok(root) => Some(root),
            Err(e) => {
                tracing::warn!(user = %creds.username, cmd, error = %e, "undecodable request");
                return EasResponse::status(StatusCode::BAD_REQUEST);
            }
        },
    };
    if limits::is_long_poll(cmd, body.as_ref()) {
        let Ok(long_poll) = state.concurrency.pings.try_acquire() else {
            return busy();
        };
        permit = long_poll;
    }
    let _permit = permit;
    let Ok(result) = tokio::time::timeout(
        limits::time_limit(cmd),
        execute(state, query, headers, creds, jmap, body),
    )
    .await
    else {
        tracing::warn!(user = %creds.username, cmd, "command took too long");
        return EasResponse::status(StatusCode::SERVICE_UNAVAILABLE);
    };
    match result {
        Ok(Outcome::Streamed(root, downloads)) => {
            tracing::debug!(user = %creds.username, cmd, streams = downloads.len(), "command done");
            EasResponse::streamed(&root, downloads)
        }
        Ok(Outcome::Wbxml(root)) => {
            tracing::debug!(user = %creds.username, cmd, "command done");
            EasResponse::wbxml(&root)
        }
        Ok(Outcome::Empty) => {
            tracing::debug!(user = %creds.username, cmd, "command done");
            EasResponse::status(StatusCode::OK)
        }
        Ok(Outcome::NotImplemented) => {
            tracing::info!(user = %creds.username, cmd, "command not implemented");
            EasResponse::status(StatusCode::NOT_IMPLEMENTED)
        }
        Ok(Outcome::Forbidden) => EasResponse::status(StatusCode::FORBIDDEN),
        Err(CommandError::BadRequest(message)) => {
            tracing::warn!(user = %creds.username, cmd, %message, "bad request");
            EasResponse::status(StatusCode::BAD_REQUEST)
        }
        // The password changed while a session was cached: verify again.
        Err(CommandError::Backend(e)) if jmap::is_unauthorized(&e) => {
            tracing::info!(user = %creds.username, cmd, "JMAP session no longer authorized");
            state.jmap.evict(creds);
            EasResponse::unauthorized()
        }
        Err(CommandError::Backend(e)) => {
            tracing::error!(user = %creds.username, cmd, error = format!("{e:#}"), "command failed");
            EasResponse::status(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// The root element and status tag of a command's response, for answers
/// that only carry a status.
fn status_tags(cmd: &str) -> Option<(crate::wbxml::Tag, crate::wbxml::Tag)> {
    match cmd {
        "FolderSync" => Some((folder_hierarchy::FolderSync, folder_hierarchy::Status)),
        "FolderCreate" => Some((folder_hierarchy::FolderCreate, folder_hierarchy::Status)),
        "FolderUpdate" => Some((folder_hierarchy::FolderUpdate, folder_hierarchy::Status)),
        "FolderDelete" => Some((folder_hierarchy::FolderDelete, folder_hierarchy::Status)),
        "Sync" => Some((airsync::Sync, airsync::Status)),
        "GetItemEstimate" => Some((item_estimate::GetItemEstimate, item_estimate::Status)),
        "MoveItems" => Some((mv::MoveItems, mv::Status)),
        "ItemOperations" => Some((io::ItemOperations, io::Status)),
        "SendMail" => Some((cm::SendMail, cm::Status)),
        "SmartReply" => Some((cm::SmartReply, cm::Status)),
        "SmartForward" => Some((cm::SmartForward, cm::Status)),
        "Search" => Some((sp::Search, sp::Status)),
        "Find" => Some((crate::wbxml::cp::find::Find, crate::wbxml::cp::find::Status)),
        "MeetingResponse" => Some((mresp::MeetingResponse, mresp::Status)),
        "ResolveRecipients" => Some((
            crate::wbxml::cp::resolve_recipients::ResolveRecipients,
            crate::wbxml::cp::resolve_recipients::Status,
        )),
        "Ping" => Some((crate::wbxml::cp::ping::Ping, crate::wbxml::cp::ping::Status)),
        _ => None,
    }
}

/// The device's EAS version from `MS-ASProtocolVersion` (or the base64
/// query form); 14.1 when it says nothing usable.
fn protocol_version(headers: &HeaderMap, query: &query::EasQuery) -> u16 {
    headers
        .get("MS-ASProtocolVersion")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (major, minor) = v.trim().split_once('.')?;
            Some(major.parse::<u16>().ok()? * 10 + minor.parse::<u16>().ok()?)
        })
        .or(query.protocol_version)
        .unwrap_or(141)
}

enum Outcome {
    Wbxml(Element),
    /// A document with attachments streamed into it (see [`stream`]).
    Streamed(Element, Vec<reqwest::Response>),
    /// Success without a body (SendMail and friends).
    Empty,
    NotImplemented,
    /// HTTP 403: the device is blocked.
    Forbidden,
}

async fn execute(
    state: &AppState,
    query: &query::EasQuery,
    headers: &HeaderMap,
    creds: &Credentials,
    jmap: &JmapClient,
    mut body: Option<Element>,
) -> Result<Outcome, CommandError> {
    let device_id = query
        .device_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| CommandError::bad("missing DeviceId"))?;
    let cmd = query.cmd.as_deref().unwrap_or_default();
    limits::check_request(device_id, query.device_type.as_deref()).map_err(CommandError::bad)?;
    // State is keyed by JMAP account, not by the login form the device uses.
    let user = jmap.account_id().map_err(CommandError::Backend)?;
    // A token is no login name to remember.
    if !creds.bearer {
        state.store.adopt_login(&creds.username, user)?;
    }

    // Any new request from a device ends its pending Ping (EAS semantics,
    // and it stops two Pings of one device from waiting side by side).
    state.push.interrupt_device(user, device_id);

    // One command at a time per device, so sync state updates cannot
    // interleave. Ping is long-lived and only reads, so it does not wait.
    let lock = state.device_locks.get(user, device_id);
    let _guard = match cmd {
        "Ping" => None,
        _ => Some(lock.lock().await),
    };

    let (known, devices) = state.store.device_count(user, device_id)?;
    if !known && devices >= state.max_devices_per_user {
        tracing::warn!(
            device_id,
            devices,
            "refusing a new device: the account has too many"
        );
        return Ok(Outcome::Forbidden);
    }
    state
        .store
        .touch_device(user, device_id, query.device_type.as_deref())?;
    state.store.note_device_login(
        user,
        device_id,
        &creds.username,
        protocol_version(headers, query),
    )?;
    // Device management (`tolk devices`): blocked and wiped devices
    // get nothing; one due for a wipe must Provision to receive it.
    match state.store.device_status(user, device_id)?.as_str() {
        "blocked" | "wiped" => {
            tracing::info!(device_id, "refusing a blocked device");
            return Ok(Outcome::Forbidden);
        }
        "wipe" | "account-wipe" if cmd != "Provision" => {
            if let Some((root, status)) = status_tags(cmd) {
                return Ok(Outcome::Wbxml(Element::new(root).child(
                    Element::with_text(status, provision::REMOTE_WIPE_REQUESTED),
                )));
            }
            return Ok(Outcome::Forbidden);
        }
        _ => {}
    }
    let version = protocol_version(headers, query);
    if version >= v16::VERSION
        && let Some(body) = &mut body
    {
        v16::incoming(body);
    }
    let login_email = if creds.username.contains('@') {
        creds.username.as_str()
    } else {
        jmap.username()
    };
    // The address the user logged in with when it is one of theirs, else
    // their first identity; aliases count as theirs in meetings.
    let addresses = jmap.own_addresses().await.unwrap_or_default();
    let email = addresses
        .iter()
        .find(|a| a.eq_ignore_ascii_case(login_email))
        .or(addresses.first())
        .map_or(login_email, String::as_str);
    let mut own = vec![email];
    own.extend(
        addresses
            .iter()
            .map(String::as_str)
            .filter(|a| !a.eq_ignore_ascii_case(email)),
    );
    if !own.iter().any(|a| a.eq_ignore_ascii_case(login_email)) {
        own.push(login_email);
    }
    let own = own.join(",");
    let req = Request {
        user,
        device_id,
        email,
        own: &own,
        policy_key: headers
            .get("X-MS-PolicyKey")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
            .or(query.policy_key),
        version,
        body,
        store: &state.store,
        persist_views: cmd != "Ping",
        caps: state.caps,
        downloads: Default::default(),
    };

    let result = dispatch(state, query, jmap, cmd, &req).await;
    // For `devices show`.
    if let Err(CommandError::Backend(e)) = &result
        && !jmap::is_unauthorized(e)
    {
        state
            .store
            .note_device_error(user, device_id, &format!("{cmd}: {e:#}"))?;
    }
    result
}

/// Runs a command for a known, allowed device.
async fn dispatch(
    state: &AppState,
    query: &query::EasQuery,
    jmap: &JmapClient,
    cmd: &str,
    req: &Request<'_>,
) -> Result<Outcome, CommandError> {
    let version = req.version;
    // Commands that read or change data need a provisioned device. Ping is
    // exempt: Gmail sends it without X-MS-PolicyKey and retries a 142
    // instantly, and it only reveals which folders changed.
    let guard = match cmd {
        "Ping" | "Settings" => None,
        other => status_tags(other),
    };
    if let Some((root, status)) = guard
        && let Some(denied) = provision::check(req, root, status)?
    {
        return Ok(Outcome::Wbxml(denied));
    }

    if let Some(op) = folder_ops::Operation::from_command(cmd) {
        return Ok(Outcome::Wbxml(folder_ops::handle(req, jmap, op).await?));
    }
    if let Some(kind) = compose::Kind::from_command(cmd) {
        return Ok(match compose::handle(req, jmap, query, kind).await? {
            Some(failure) => Outcome::Wbxml(failure),
            None => Outcome::Empty,
        });
    }
    let response = match cmd {
        "Provision" => provision::handle(req)?,
        "Settings" => settings::handle(req, jmap).await?,
        "FolderSync" => folder_sync::handle(req, jmap).await?,
        "Sync" => match sync::handle(req, jmap, &state.push).await? {
            Some(response) => response,
            None => return Ok(Outcome::Empty),
        },
        "GetItemEstimate" => sync::estimate(req, jmap).await?,
        "MoveItems" => move_items::handle(req, jmap).await?,
        "ItemOperations" => item_operations::handle(req, jmap).await?,
        "Search" => search::handle(req, jmap).await?,
        "Find" => find::handle(req, jmap).await?,
        "MeetingResponse" => meeting_response::handle(req, jmap).await?,
        "ResolveRecipients" => resolve_recipients::handle(req, jmap).await?,
        "Ping" => ping::handle(req, jmap, &state.push).await?,
        _ => return Ok(Outcome::NotImplemented),
    };
    let mut response = response;
    if version >= v16::VERSION {
        v16::outgoing(&mut response);
    }
    let downloads = req.downloads.take();
    if downloads.is_empty() {
        Ok(Outcome::Wbxml(response))
    } else {
        Ok(Outcome::Streamed(response, downloads))
    }
}
