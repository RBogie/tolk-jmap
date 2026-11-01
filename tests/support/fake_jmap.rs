//! An in-process, stateful stand-in for the JMAP server (RFC 8620, 8621 and
//! the contacts and calendars drafts), covering the methods Tolk uses.
//!
//! One user with one account, plus optionally a second account shared with
//! them (calendars and address books, see [`FakeJmap::share`]). Every data
//! type keeps a change log, so
//! `<type>/changes` works like a real server's, and every change is pushed to
//! open EventSource streams. Faults can be injected per method or for whole
//! requests.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Map, Value, json};

pub const USER: &str = "test@example.com";
pub const PASSWORD: &str = "secret";
/// Another login name for the same account.
pub const ALIAS: &str = "test";
pub const ACCOUNT: &str = "a1";
/// An account another user shares with the test user.
pub const SHARED_ACCOUNT: &str = "a2";
/// An OAuth access token the server accepts for the test user.
pub const TOKEN: &str = "test-access-token";
pub const SHARED_OWNER: &str = "Owner Two";

const TYPES: &[&str] = &[
    "Mailbox",
    "Email",
    "Identity",
    "EmailSubmission",
    "AddressBook",
    "ContactCard",
    "Calendar",
    "CalendarEvent",
    "VacationResponse",
];

#[derive(Default)]
pub struct Model {
    /// Objects per type, by id.
    pub objects: HashMap<String, BTreeMap<String, Value>>,
    /// Current state per type; states are decimal counters.
    states: HashMap<String, u64>,
    /// Per type: (state after the change, id, kind).
    log: HashMap<String, Vec<(u64, String, Change)>>,
    /// Changes older than this state are forgotten (`cannotCalculateChanges`).
    pub oldest_state: HashMap<String, u64>,
    pub blobs: HashMap<String, (Vec<u8>, String)>,
    next_id: u64,
    /// Method name to (error type, remaining count).
    method_faults: HashMap<String, (String, usize)>,
    /// HTTP status for the next N API requests.
    http_faults: Vec<StatusCode>,
    /// Every method call received, in order.
    pub calls: Vec<String>,
    /// Every EmailSubmission created, as sent.
    pub submissions: Vec<Value>,
    /// Data types this account advertises but does not serve
    /// (`accountNotSupportedByMethod`, as Stalwart answers for a shared
    /// account without shared address books).
    pub unsupported: Vec<String>,
    /// What `CalendarEvent/parse` returns per blob id; other blobs are not
    /// parsable.
    pub parsed: HashMap<String, Value>,
    /// Downloads answer `Range` requests with the whole blob, as servers
    /// without range support do.
    pub ignore_ranges: bool,
    /// Every `Range` header downloads received.
    pub ranges: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Created,
    Updated,
    Destroyed,
}

impl Model {
    fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    fn state(&self, data_type: &str) -> u64 {
        self.states.get(data_type).copied().unwrap_or(0)
    }

    fn record(&mut self, data_type: &str, id: &str, change: Change) {
        let state = self.state(data_type) + 1;
        self.states.insert(data_type.to_owned(), state);
        self.log
            .entry(data_type.to_owned())
            .or_default()
            .push((state, id.to_owned(), change));
    }

    fn table(&mut self, data_type: &str) -> &mut BTreeMap<String, Value> {
        self.objects.entry(data_type.to_owned()).or_default()
    }

    /// Stores a new object and returns its id.
    pub fn insert(&mut self, data_type: &str, mut object: Value) -> String {
        let id = match object["id"].as_str() {
            Some(id) => id.to_owned(),
            None => {
                let prefix = data_type[..1].to_lowercase();
                self.new_id(&prefix)
            }
        };
        object["id"] = json!(id);
        self.table(data_type).insert(id.clone(), object);
        self.record(data_type, &id, Change::Created);
        id
    }

    /// Applies a JMAP patch object to an existing object.
    pub fn patch(&mut self, data_type: &str, id: &str, patch: &Map<String, Value>) -> bool {
        let Some(object) = self.table(data_type).get_mut(id) else {
            return false;
        };
        for (path, value) in patch {
            apply_patch(object, path, value.clone());
        }
        self.record(data_type, id, Change::Updated);
        true
    }

    pub fn remove(&mut self, data_type: &str, id: &str) -> bool {
        if self.table(data_type).remove(id).is_none() {
            return false;
        }
        self.record(data_type, id, Change::Destroyed);
        true
    }

    pub fn get(&self, data_type: &str, id: &str) -> Option<&Value> {
        self.objects.get(data_type).and_then(|t| t.get(id))
    }

    pub fn all(&self, data_type: &str) -> Vec<&Value> {
        self.objects
            .get(data_type)
            .map(|t| t.values().collect())
            .unwrap_or_default()
    }

    fn type_states(&self) -> Vec<u64> {
        TYPES.iter().map(|t| self.state(t)).collect()
    }

    /// The data types whose state differs from `before`.
    fn changed_types(&self, before: &[u64]) -> Vec<String> {
        TYPES
            .iter()
            .zip(before)
            .filter(|(t, state)| self.state(t) != **state)
            .map(|(t, _)| (*t).to_owned())
            .collect()
    }

    /// Makes `<type>/changes` fail for every state before the current one.
    pub fn forget_changes(&mut self, data_type: &str) {
        let state = self.state(data_type);
        self.oldest_state.insert(data_type.to_owned(), state);
    }
}

/// Sets `path` (slash separated, RFC 6901 escaping) to `value`; null removes.
fn apply_patch(object: &mut Value, path: &str, value: Value) {
    let parts: Vec<String> = path
        .split('/')
        .map(|p| p.replace("~1", "/").replace("~0", "~"))
        .collect();
    let mut target = object;
    for part in &parts[..parts.len() - 1] {
        if !target[part.as_str()].is_object() {
            target[part.as_str()] = json!({});
        }
        target = &mut target[part.as_str()];
    }
    let last = parts.last().unwrap();
    let map = target.as_object_mut().expect("patch target is an object");
    if value.is_null() {
        map.remove(last);
    } else {
        map.insert(last.clone(), value);
    }
}

/// Changed data types, per account.
type Event = (&'static str, Vec<String>);

pub struct FakeJmap {
    pub url: String,
    pub model: Arc<Mutex<Model>>,
    /// The shared account's data.
    pub other: Arc<Mutex<Model>>,
    /// Whether the shared account is visible, and if so read-only.
    sharing: Arc<Mutex<Option<bool>>>,
    events: tokio::sync::broadcast::Sender<Event>,
}

#[derive(Clone)]
struct Shared {
    url: String,
    model: Arc<Mutex<Model>>,
    other: Arc<Mutex<Model>>,
    sharing: Arc<Mutex<Option<bool>>>,
    events: tokio::sync::broadcast::Sender<Event>,
}

impl FakeJmap {
    /// Starts the server with an Inbox, Sent, Drafts and Trash, one identity,
    /// one address book and one calendar.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let model = Arc::new(Mutex::new(Model::default()));
        {
            let mut m = model.lock().unwrap();
            for (id, name, role, order) in [
                ("inbox", "Inbox", Some("inbox"), 1),
                ("sent", "Sent", Some("sent"), 2),
                ("drafts", "Drafts", Some("drafts"), 3),
                ("trash", "Trash", Some("trash"), 4),
            ] {
                m.insert(
                    "Mailbox",
                    json!({ "id": id, "name": name, "parentId": null, "role": role, "sortOrder": order }),
                );
            }
            m.insert(
                "Identity",
                json!({ "id": "id1", "name": "Test", "email": USER }),
            );
            m.insert(
                "AddressBook",
                json!({ "id": "ab1", "name": "Contacts", "isDefault": true }),
            );
            m.insert(
                "Calendar",
                json!({ "id": "cal1", "name": "Calendar", "isDefault": true }),
            );
            m.insert(
                "VacationResponse",
                json!({ "id": "singleton", "isEnabled": false, "fromDate": null, "toDate": null,
                        "subject": null, "textBody": null, "htmlBody": null }),
            );
        }
        let (events, _) = tokio::sync::broadcast::channel(64);
        let other = Arc::new(Mutex::new(Model::default()));
        let sharing = Arc::new(Mutex::new(None));
        let shared = Shared {
            url: url.clone(),
            model: model.clone(),
            other: other.clone(),
            sharing: sharing.clone(),
            events: events.clone(),
        };
        let app = Router::new()
            .route("/.well-known/jmap", get(session))
            .route("/api", post(api))
            .route("/upload/{account}/", post(upload))
            .route("/download/{account}/{blob}/{name}", get(download))
            .route("/events", get(event_source))
            .with_state(shared);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            model,
            other,
            sharing,
            events,
        }
    }

    /// Shows the test user the second account, with an address book `sab1`,
    /// a calendar `scal1` and mailboxes `inbox` (the same id as the user's
    /// own, as Stalwart reuses ids across accounts) and `sproj` below it
    /// (read-only: `isReadOnly` on the account).
    pub fn share(&self, read_only: bool) {
        *self.sharing.lock().unwrap() = Some(read_only);
        self.change_shared(|m| {
            if m.get("AddressBook", "sab1").is_none() {
                m.insert(
                    "AddressBook",
                    json!({ "id": "sab1", "name": "Team", "isDefault": true }),
                );
                m.insert(
                    "Calendar",
                    json!({ "id": "scal1", "name": "Team", "isDefault": true }),
                );
                m.insert(
                    "Mailbox",
                    json!({ "id": "inbox", "name": "Inbox", "parentId": null, "role": "inbox", "sortOrder": 1 }),
                );
                m.insert(
                    "Mailbox",
                    json!({ "id": "sproj", "name": "Projects", "parentId": "inbox", "role": null, "sortOrder": 2 }),
                );
            }
        });
    }

    /// Runs `f` on the model and pushes a state change to EventSource
    /// listeners, like a change made by another client.
    pub fn change<T>(&self, f: impl FnOnce(&mut Model) -> T) -> T {
        self.change_in(ACCOUNT, &self.model, f)
    }

    /// [`Self::change`] for the shared account.
    pub fn change_shared<T>(&self, f: impl FnOnce(&mut Model) -> T) -> T {
        self.change_in(SHARED_ACCOUNT, &self.other, f)
    }

    fn change_in<T>(
        &self,
        account: &'static str,
        model: &Mutex<Model>,
        f: impl FnOnce(&mut Model) -> T,
    ) -> T {
        let mut model = model.lock().unwrap();
        let before = model.type_states();
        let result = f(&mut model);
        let changed = model.changed_types(&before);
        drop(model);
        if !changed.is_empty() {
            let _ = self.events.send((account, changed));
        }
        result
    }

    /// Reads the shared account's model.
    pub fn read_shared<T>(&self, f: impl FnOnce(&Model) -> T) -> T {
        f(&self.other.lock().unwrap())
    }

    /// Reads the model without notifying anyone.
    pub fn read<T>(&self, f: impl FnOnce(&Model) -> T) -> T {
        f(&self.model.lock().unwrap())
    }

    /// Adds a message to a mailbox and returns its id.
    pub fn add_email(&self, mailbox: &str, subject: &str, body: &str) -> String {
        self.change(|m| email_into(m, mailbox, subject, body))
    }

    /// [`Self::add_email`] in the shared account.
    pub fn add_shared_email(&self, mailbox: &str, subject: &str, body: &str) -> String {
        self.change_shared(|m| email_into(m, mailbox, subject, body))
    }

    /// Makes the next `times` calls of `method` fail with a method error.
    pub fn fail_method(&self, method: &str, error_type: &str, times: usize) {
        self.model
            .lock()
            .unwrap()
            .method_faults
            .insert(method.to_owned(), (error_type.to_owned(), times));
    }

    /// Makes the next API requests answer with these HTTP statuses.
    pub fn fail_requests(&self, statuses: &[StatusCode]) {
        self.model.lock().unwrap().http_faults = statuses.to_vec();
    }

    /// Method calls received so far, and clears the list.
    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.model.lock().unwrap().calls)
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    use base64::Engine;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if given == Some(format!("Bearer {TOKEN}").as_str()) {
        return true;
    }
    [USER, ALIAS].iter().any(|login| {
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{login}:{PASSWORD}"))
        );
        given == Some(expected.as_str())
    })
}

async fn session(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let url = &shared.url;
    let capabilities = json!({
        "urn:ietf:params:jmap:core": { "maxObjectsInGet": 500, "maxSizeUpload": 50_000_000 },
        "urn:ietf:params:jmap:mail": {},
        "urn:ietf:params:jmap:submission": {},
        "urn:ietf:params:jmap:contacts": {},
        "urn:ietf:params:jmap:calendars": {},
        "urn:ietf:params:jmap:calendars:parse": {},
        "urn:ietf:params:jmap:vacationresponse": {},
    });
    let primary: Map<String, Value> = capabilities
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| !k.ends_with(":core"))
        .map(|k| (k.clone(), json!(ACCOUNT)))
        .collect();
    let own_capabilities: Map<String, Value> =
        primary.keys().map(|k| (k.clone(), json!({}))).collect();
    let mut accounts = json!({ ACCOUNT: {
        "name": USER, "isPersonal": true, "isReadOnly": false,
        "accountCapabilities": own_capabilities,
    } });
    if let Some(read_only) = *shared.sharing.lock().unwrap() {
        accounts[SHARED_ACCOUNT] = json!({
            "name": SHARED_OWNER, "isPersonal": false, "isReadOnly": read_only,
            "accountCapabilities": {
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:contacts": {},
                "urn:ietf:params:jmap:calendars": {},
            },
        });
    }
    Json(json!({
        "capabilities": capabilities,
        "accounts": accounts,
        "primaryAccounts": primary,
        "username": USER,
        "apiUrl": format!("{url}/api"),
        "downloadUrl": format!("{url}/download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}"),
        "uploadUrl": format!("{url}/upload/{{accountId}}/"),
        "eventSourceUrl": format!("{url}/events?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
        "state": "s1",
    }))
    .into_response()
}

async fn upload(State(shared): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let mut model = shared.model.lock().unwrap();
    let id = model.new_id("blob-");
    let size = body.len();
    model
        .blobs
        .insert(id.clone(), (body.to_vec(), mime.clone()));
    Json(json!({ "accountId": ACCOUNT, "blobId": id, "type": mime, "size": size })).into_response()
}

async fn download(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Path((account, blob, _name)): Path<(String, String, String)>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut model = if account == SHARED_ACCOUNT {
        shared.other.lock().unwrap()
    } else {
        shared.model.lock().unwrap()
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(range) = &range {
        model.ranges.push(range.clone());
    }
    let Some((data, mime)) = model.blobs.get(&blob) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let wanted = range.filter(|_| !model.ignore_ranges).and_then(|r| {
        let (start, end) = r.strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
    });
    match wanted {
        Some((start, _)) if start >= data.len() => {
            StatusCode::RANGE_NOT_SATISFIABLE.into_response()
        }
        Some((start, end)) => {
            let end = end.min(data.len() - 1);
            let content_range = format!("bytes {start}-{end}/{}", data.len());
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime.clone()),
                    (header::CONTENT_RANGE, content_range),
                ],
                data[start..=end].to_vec(),
            )
                .into_response()
        }
        None => ([(header::CONTENT_TYPE, mime.clone())], data.clone()).into_response(),
    }
}

async fn event_source(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let receiver = shared.events.subscribe();
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        let (account, changed): (&str, Map<String, Value>) = match receiver.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            Ok((account, types)) => (
                account,
                types.into_iter().map(|t| (t, json!("new"))).collect(),
            ),
            // A lagged receiver still has to hear that something changed.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => (
                ACCOUNT,
                TYPES
                    .iter()
                    .map(|t| ((*t).to_owned(), json!("new")))
                    .collect(),
            ),
        };
        let data = json!({ "@type": "StateChange", "changed": { account: changed } });
        let event = format!("event: state\ndata: {data}\n\n");
        Some((Ok::<_, std::io::Error>(Bytes::from(event)), receiver))
    });
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn api(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (responses, changed, changed_shared) = {
        let mut model = shared.model.lock().unwrap();
        let mut other = shared.other.lock().unwrap();
        let read_only = *shared.sharing.lock().unwrap() == Some(true);
        if !model.http_faults.is_empty() {
            let status = model.http_faults.remove(0);
            return status.into_response();
        }
        let before = model.type_states();
        let before_shared = other.type_states();
        let mut responses: Vec<Value> = Vec::new();
        let mut created_ids: HashMap<String, String> = HashMap::new();
        for call in request["methodCalls"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let method = call[0].as_str().unwrap_or_default().to_owned();
            let call_id = call[2].clone();
            model.calls.push(method.clone());
            let mut args = call[1].clone();
            resolve_back_references(&mut args, &responses);
            let in_shared = args["accountId"] == SHARED_ACCOUNT;
            let (name, result) = match model.method_faults.get_mut(&method) {
                Some((error, remaining)) if *remaining > 0 => {
                    *remaining -= 1;
                    ("error".to_owned(), json!({ "type": error.clone() }))
                }
                _ if method == "Email/copy" => {
                    let (from, to) = if args["fromAccountId"] == SHARED_ACCOUNT {
                        (&mut *other, &mut *model)
                    } else {
                        (&mut *model, &mut *other)
                    };
                    ("Email/copy".to_owned(), email_copy(from, to, &args))
                }
                _ if in_shared && read_only && method.ends_with("/set") => {
                    ("error".to_owned(), json!({ "type": "accountReadOnly" }))
                }
                _ if in_shared => dispatch(&mut other, &method, args, &mut created_ids),
                _ => dispatch(&mut model, &method, args, &mut created_ids),
            };
            responses.push(json!([name, result, call_id]));
        }
        (
            responses,
            model.changed_types(&before),
            other.changed_types(&before_shared),
        )
    };
    if !changed.is_empty() {
        let _ = shared.events.send((ACCOUNT, changed));
    }
    if !changed_shared.is_empty() {
        let _ = shared.events.send((SHARED_ACCOUNT, changed_shared));
    }
    Json(json!({ "methodResponses": responses, "sessionState": "s1" })).into_response()
}

/// Replaces `"#name": { resultOf, name, path }` arguments with the value
/// from an earlier response (only `/ids` and `/list/*/id` paths).
fn resolve_back_references(args: &mut Value, responses: &[Value]) {
    let Some(map) = args.as_object_mut() else {
        return;
    };
    let refs: Vec<String> = map.keys().filter(|k| k.starts_with('#')).cloned().collect();
    for key in refs {
        let reference = map.remove(&key).unwrap();
        let source = responses
            .iter()
            .find(|r| r[2] == reference["resultOf"])
            .map(|r| r[1].clone())
            .unwrap_or(Value::Null);
        let value = match reference["path"].as_str() {
            Some("/ids") => source["ids"].clone(),
            Some("/list/*/id") => json!(
                source["list"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| o["id"].clone())
                    .collect::<Vec<_>>()
            ),
            other => panic!("unsupported back-reference path {other:?}"),
        };
        map.insert(key[1..].to_owned(), value);
    }
}

fn dispatch(
    model: &mut Model,
    method: &str,
    args: Value,
    created_ids: &mut HashMap<String, String>,
) -> (String, Value) {
    let (data_type, operation) = method.split_once('/').unwrap_or((method, ""));
    let ok = |result: Value| (method.to_owned(), result);
    if model.unsupported.iter().any(|t| t == data_type) {
        return (
            "error".to_owned(),
            json!({ "type": "accountNotSupportedByMethod" }),
        );
    }
    match (data_type, operation) {
        ("Principal", _) => ("error".to_owned(), json!({ "type": "forbidden" })),
        ("Email", "import") => ok(email_import(model, &args, created_ids)),
        ("EmailSubmission", "set") => ok(submission_set(model, &args, created_ids)),
        ("CalendarEvent", "parse") => {
            let (parsed, not_parsable): (Vec<&Value>, Vec<&Value>) = args["blobIds"]
                .as_array()
                .into_iter()
                .flatten()
                .partition(|id| model.parsed.contains_key(id.as_str().unwrap_or_default()));
            let parsed: Map<String, Value> = parsed
                .into_iter()
                .filter_map(Value::as_str)
                .map(|id| (id.to_owned(), json!([model.parsed[id]])))
                .collect();
            ok(json!({
                "accountId": ACCOUNT,
                "parsed": parsed,
                "notParsable": not_parsable,
            }))
        }
        (t, "get") if TYPES.contains(&t) => ok(get_objects(model, t, &args)),
        (t, "query") if TYPES.contains(&t) => ok(query(model, t, &args)),
        (t, "changes") if TYPES.contains(&t) => match changes(model, t, &args) {
            Ok(result) => ok(result),
            Err(error) => ("error".to_owned(), json!({ "type": error })),
        },
        (t, "set") if TYPES.contains(&t) => ok(set_objects(model, t, &args, created_ids)),
        _ => ("error".to_owned(), json!({ "type": "unknownMethod" })),
    }
}

fn get_objects(model: &Model, data_type: &str, args: &Value) -> Value {
    let properties: Option<Vec<&str>> = args["properties"]
        .as_array()
        .map(|p| p.iter().filter_map(Value::as_str).collect());
    let select = |object: &Value| -> Value {
        match &properties {
            None => object.clone(),
            Some(props) => {
                let mut out = Map::new();
                out.insert("id".to_owned(), object["id"].clone());
                for p in props {
                    out.insert(
                        (*p).to_owned(),
                        object.get(*p).cloned().unwrap_or(Value::Null),
                    );
                }
                Value::Object(out)
            }
        }
    };
    let (list, not_found): (Vec<Value>, Vec<Value>) = match args["ids"].as_array() {
        None => (
            model.all(data_type).into_iter().map(select).collect(),
            vec![],
        ),
        Some(ids) => {
            let mut list = Vec::new();
            let mut missing = Vec::new();
            for id in ids {
                match id.as_str().and_then(|id| model.get(data_type, id)) {
                    Some(object) => list.push(select(object)),
                    None => missing.push(id.clone()),
                }
            }
            (list, missing)
        }
    };
    json!({
        "accountId": ACCOUNT,
        "state": model.state(data_type).to_string(),
        "list": list,
        "notFound": not_found,
    })
}

fn matches(object: &Value, filter: &Value) -> bool {
    let Some(filter) = filter.as_object() else {
        return true;
    };
    if let Some(operator) = filter.get("operator").and_then(Value::as_str) {
        let mut results = filter["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| matches(object, c));
        return match operator {
            "AND" => results.all(|r| r),
            "OR" => results.any(|r| r),
            _ => !results.any(|r| r),
        };
    }
    filter.iter().all(|(key, value)| match key.as_str() {
        "inMailbox" => object["mailboxIds"][value.as_str().unwrap_or_default()] == true,
        "inAddressBook" => object["addressBookIds"][value.as_str().unwrap_or_default()] == true,
        "inCalendar" => object["calendarIds"][value.as_str().unwrap_or_default()] == true,
        "uid" => object["uid"] == *value,
        "after" => object["receivedAt"]
            .as_str()
            .is_none_or(|d| d >= value.as_str().unwrap_or_default()),
        "before" => object["receivedAt"]
            .as_str()
            .is_none_or(|d| d < value.as_str().unwrap_or_default()),
        "text" | "subject" => {
            let needle = value.as_str().unwrap_or_default().to_lowercase();
            object["subject"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&needle)
                || object["bodyValues"]
                    .to_string()
                    .to_lowercase()
                    .contains(&needle)
        }
        "hasKeyword" => object["keywords"][value.as_str().unwrap_or_default()] == true,
        "notKeyword" => object["keywords"][value.as_str().unwrap_or_default()] != true,
        _ => true,
    })
}

fn query(model: &Model, data_type: &str, args: &Value) -> Value {
    let mut found: Vec<&Value> = model
        .all(data_type)
        .into_iter()
        .filter(|o| matches(o, &args["filter"]))
        .collect();
    if data_type == "Email" {
        found.sort_by(|a, b| {
            b["receivedAt"]
                .as_str()
                .cmp(&a["receivedAt"].as_str())
                .then_with(|| b["id"].as_str().cmp(&a["id"].as_str()))
        });
    }
    let total = found.len();
    let position = args["position"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().map_or(usize::MAX, |l| l as usize);
    let ids: Vec<Value> = found
        .into_iter()
        .skip(position)
        .take(limit)
        .map(|o| o["id"].clone())
        .collect();
    json!({
        "accountId": ACCOUNT,
        "queryState": model.state(data_type).to_string(),
        "canCalculateChanges": false,
        "position": position,
        "ids": ids,
        "total": total,
    })
}

fn changes(model: &Model, data_type: &str, args: &Value) -> Result<Value, &'static str> {
    let since: u64 = args["sinceState"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or("cannotCalculateChanges")?;
    let oldest = model.oldest_state.get(data_type).copied().unwrap_or(0);
    if since < oldest || since > model.state(data_type) {
        return Err("cannotCalculateChanges");
    }
    let max = args["maxChanges"]
        .as_u64()
        .map_or(usize::MAX, |m| m as usize);
    let mut first: BTreeMap<&str, Change> = BTreeMap::new();
    let mut last: BTreeMap<&str, Change> = BTreeMap::new();
    let mut new_state = since;
    let log = model
        .log
        .get(data_type)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (state, id, change) in log.iter().filter(|(s, _, _)| *s > since) {
        if !first.contains_key(id.as_str()) && first.len() == max {
            break;
        }
        first.entry(id).or_insert(*change);
        last.insert(id, *change);
        new_state = *state;
    }
    let (mut created, mut updated, mut destroyed) = (vec![], vec![], vec![]);
    for (id, first_change) in &first {
        match (first_change, last[id]) {
            (Change::Created, Change::Destroyed) => {}
            (Change::Created, _) => created.push(*id),
            (_, Change::Destroyed) => destroyed.push(*id),
            _ => updated.push(*id),
        }
    }
    Ok(json!({
        "accountId": ACCOUNT,
        "oldState": since.to_string(),
        "newState": new_state.to_string(),
        "hasMoreChanges": new_state < model.state(data_type),
        "created": created,
        "updated": updated,
        "destroyed": destroyed,
    }))
}

fn set_objects(
    model: &mut Model,
    data_type: &str,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
) -> Value {
    let old_state = model.state(data_type).to_string();
    let mut created = Map::new();
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed = Vec::new();
    let mut not_destroyed = Map::new();
    for (creation_id, object) in args["create"].as_object().into_iter().flatten() {
        let id = model.insert(data_type, object.clone());
        created_ids.insert(creation_id.clone(), id.clone());
        created.insert(creation_id.clone(), json!({ "id": id }));
    }
    for (id, patch) in args["update"].as_object().into_iter().flatten() {
        let patch = patch.as_object().cloned().unwrap_or_default();
        if model.patch(data_type, id, &patch) {
            updated.insert(id.clone(), Value::Null);
        } else {
            not_updated.insert(id.clone(), json!({ "type": "notFound" }));
        }
    }
    for id in args["destroy"].as_array().into_iter().flatten() {
        let id = id.as_str().unwrap_or_default();
        if model.remove(data_type, id) {
            destroyed.push(json!(id));
        } else {
            not_destroyed.insert(id.to_owned(), json!({ "type": "notFound" }));
        }
    }
    json!({
        "accountId": ACCOUNT,
        "oldState": old_state,
        "newState": model.state(data_type).to_string(),
        "created": created,
        "updated": updated,
        "destroyed": destroyed,
        "notCreated": {},
        "notUpdated": not_updated,
        "notDestroyed": not_destroyed,
    })
}

fn email_import(
    model: &mut Model,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
) -> Value {
    let mut created = Map::new();
    let mut not_created = Map::new();
    for (creation_id, spec) in args["emails"].as_object().into_iter().flatten() {
        let blob_id = spec["blobId"].as_str().unwrap_or_default();
        let Some((raw, _)) = model.blobs.get(blob_id) else {
            not_created.insert(creation_id.clone(), json!({ "type": "blobNotFound" }));
            continue;
        };
        let parsed_attachments: Vec<(Vec<u8>, Value)> = mail_parser::MessageParser::default()
            .parse(raw)
            .map(|m| {
                m.attachments()
                    .enumerate()
                    .map(|(n, part)| {
                        let mime = part
                            .content_type()
                            .map(|ct| match ct.subtype() {
                                Some(sub) => format!("{}/{sub}", ct.ctype()),
                                None => ct.ctype().to_owned(),
                            })
                            .unwrap_or_else(|| "application/octet-stream".to_owned());
                        let bytes = part.encoding().decode(part.raw_body()).into_owned();
                        let meta = json!({
                            "partId": format!("att{n}"),
                            "blobId": format!("{blob_id}-att{n}"),
                            "size": bytes.len(),
                            "name": part.attachment_name(),
                            "type": mime,
                            "cid": part.content_id(),
                            "disposition": part.content_disposition().map(|d| d.ctype().to_owned()),
                        });
                        (bytes, meta)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let raw = String::from_utf8_lossy(raw).into_owned();
        let mut attachments = Vec::new();
        for (bytes, meta) in parsed_attachments {
            let mime = meta["type"].as_str().unwrap_or_default().to_owned();
            model
                .blobs
                .insert(meta["blobId"].as_str().unwrap().to_owned(), (bytes, mime));
            attachments.push(meta);
        }
        let subject = raw
            .lines()
            .find_map(|l| l.strip_prefix("Subject: "))
            .unwrap_or_default()
            .to_owned();
        // Addresses only, which is all the tests look at.
        let addresses = |header: &str| -> Value {
            let Some(line) = raw.lines().find_map(|l| l.strip_prefix(header)) else {
                return Value::Null;
            };
            json!(
                line.split(',')
                    .filter_map(|part| {
                        let address = part.rsplit('<').next()?.trim().trim_end_matches('>');
                        address
                            .contains('@')
                            .then(|| json!({ "name": null, "email": address }))
                    })
                    .collect::<Vec<_>>()
            )
        };
        let id = model.insert(
            "Email",
            json!({
                "blobId": blob_id,
                "mailboxIds": spec["mailboxIds"],
                "keywords": spec["keywords"],
                "size": raw.len(),
                "receivedAt": "2026-10-30T12:00:00Z",
                "subject": subject,
                "from": null, "to": addresses("To: "), "cc": addresses("Cc: "), "replyTo": null,
                "messageId": null, "references": null,
                "textBody": [], "htmlBody": [], "attachments": attachments, "bodyValues": {},
            }),
        );
        created_ids.insert(creation_id.clone(), id.clone());
        created.insert(creation_id.clone(), json!({ "id": id, "blobId": blob_id }));
    }
    json!({ "accountId": ACCOUNT, "created": created, "notCreated": not_created })
}

/// `Email/copy` between the two accounts (also `onSuccessDestroyOriginal`).
fn email_copy(from: &mut Model, to: &mut Model, args: &Value) -> Value {
    let mut created = Map::new();
    let mut not_created = Map::new();
    for (creation_id, spec) in args["create"].as_object().into_iter().flatten() {
        let id = spec["id"].as_str().unwrap_or_default();
        let Some(mut email) = from.get("Email", id).cloned() else {
            not_created.insert(creation_id.clone(), json!({ "type": "notFound" }));
            continue;
        };
        email.as_object_mut().unwrap().remove("id");
        email["mailboxIds"] = spec["mailboxIds"].clone();
        let new_id = to.insert("Email", email);
        if args["onSuccessDestroyOriginal"] == true {
            from.remove("Email", id);
        }
        created.insert(creation_id.clone(), json!({ "id": new_id }));
    }
    json!({ "accountId": args["accountId"], "created": created, "notCreated": not_created })
}

fn submission_set(
    model: &mut Model,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
) -> Value {
    let mut created = Map::new();
    let mut submitted: HashMap<String, String> = HashMap::new();
    for (creation_id, spec) in args["create"].as_object().into_iter().flatten() {
        let mut spec = spec.clone();
        if let Some(reference) = spec["emailId"].as_str().and_then(|r| r.strip_prefix('#')) {
            spec["emailId"] = json!(created_ids.get(reference));
        }
        model.submissions.push(spec.clone());
        let id = model.insert("EmailSubmission", spec.clone());
        submitted.insert(
            format!("#{creation_id}"),
            spec["emailId"].as_str().unwrap_or_default().to_owned(),
        );
        created.insert(creation_id.clone(), json!({ "id": id }));
    }
    for reference in args["onSuccessDestroyEmail"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(email) = reference.as_str().and_then(|r| submitted.get(r)) {
            model.remove("Email", email);
        }
    }
    json!({ "accountId": ACCOUNT, "created": created, "notCreated": {} })
}

/// Stores a plain text message in `mailbox` of `m` and returns its id.
fn email_into(m: &mut Model, mailbox: &str, subject: &str, body: &str) -> String {
    let n = m.next_id + 1;
    let blob = format!("blob-msg-{n}");
    let raw = format!(
        "From: Sender <sender@example.org>\r\nTo: {USER}\r\nSubject: {subject}\r\n\
         Message-ID: <m{n}@example.org>\r\nContent-Type: text/plain\r\n\r\n{body}\r\n"
    );
    m.blobs.insert(
        blob.clone(),
        (raw.into_bytes(), "message/rfc822".to_owned()),
    );
    m.insert(
        "Email",
        json!({
            "blobId": blob,
            "mailboxIds": { mailbox: true },
            "keywords": {},
            "size": body.len() + 200,
            "receivedAt": format!("2026-10-{:02}T10:{:02}:00Z", 1 + n % 28, n % 60),
            "subject": subject,
            "from": [{ "name": "Sender", "email": "sender@example.org" }],
            "to": [{ "name": null, "email": USER }],
            "cc": null,
            "replyTo": null,
            "messageId": [format!("m{n}@example.org")],
            "references": null,
            "textBody": [{ "partId": "1", "blobId": format!("{blob}-1"), "size": body.len(), "name": null, "type": "text/plain", "cid": null, "disposition": null }],
            "htmlBody": [{ "partId": "1", "blobId": format!("{blob}-1"), "size": body.len(), "name": null, "type": "text/plain", "cid": null, "disposition": null }],
            "attachments": [],
            "bodyValues": { "1": { "value": body, "isTruncated": false } },
            "threadId": format!("t{n}"),
            "preview": body,
        }),
    )
}
