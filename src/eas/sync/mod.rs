//! Sync (MS-ASCMD 2.2.1.21) for mail, contact and calendar folders, and
//! GetItemEstimate.
//!
//! A request first applies the device's own changes (`<Commands>`: read and
//! flag changes, deletes, fetches) to JMAP and to the device's snapshot.
//! Then the folder's current state (see [`view`]) is diffed against that
//! snapshot. At most WindowSize changes are sent, and the snapshot advances
//! by exactly what was sent.
//!
//! The last full request is kept per device: an empty Sync repeats it and a
//! `<Partial>` one is merged into it. With `<Wait>` or `<HeartbeatInterval>`
//! a Sync that finds nothing waits for changes like Ping does (iOS uses this
//! for push) and answers with an empty body when the time runs out.

use std::{collections::HashSet, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    CommandError, CommandResult, Request, calendar, contacts,
    device_changes::{self, item_status},
    drafts,
    email::{self, BodyPreference},
    view::{self, FLAGGED, READ},
};
use crate::{
    jmap::{self, JmapClient, SetResult},
    push::PushHub,
    store::{CollectionSyncState, SyncRepeats},
    wbxml::{
        self, Element, Node, Tag,
        cp::{airsync, airsyncbase, email as email_cp, item_estimate},
    },
};

mod estimate;
mod mail;
mod request;

pub use estimate::estimate;
use mail::{ClientCommands, mail_commands};
pub(super) use mail::{move_patch, record_move};
use request::{full_request, wait_time};

mod status {
    pub const OK: &str = "1";
    pub const INVALID_SYNC_KEY: &str = "3";
    pub const PROTOCOL_ERROR: &str = "4";
    pub const SERVER_ERROR: &str = "5";
    pub const CONVERSION_ERROR: &str = "6";
    pub const OBJECT_NOT_FOUND: &str = "8";
    pub const HIERARCHY_CHANGED: &str = "12";
    pub const INCOMPLETE_REQUEST: &str = "13";
    pub const INVALID_WAIT: &str = "14";
}

/// How long a Sync may wait for changes, in seconds (like Ping).
const MIN_WAIT: u64 = 60;
const MAX_WAIT: u64 = super::ping::MAX_HEARTBEAT;

const DEFAULT_WINDOW: usize = 100;
const MAX_WINDOW: usize = 512;

/// A device that sends the SyncKey of a response again this often in a row
/// (each within [`LOOP_GAP`] of the last) cannot take an item in it: the
/// items of that response are then sent one per response, and one the
/// device repeats this often too is quarantined. One repeat is a lost
/// response and is answered as usual; afterwards normal windows resume.
const LOOP_REPEATS: u32 = 2;
const LOOP_GAP: i64 = 30 * 60;

pub(super) use crate::store::Snapshot;

/// Options are sticky: a device sends them once and the server remembers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOptions {
    pub filter_type: u8,
    pub body_preferences: Vec<BodyPreference>,
    /// The contact fields the device manages (`<Supported>`, sent with
    /// SyncKey 0), as tag names; `None` when it sent none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<Vec<String>>,
}

impl SyncOptions {
    /// The `<Options>` of a collection; SMS options are ignored.
    fn from_collection(collection: &Element) -> Option<Self> {
        let options = collection
            .find_all(airsync::Options)
            .find(|o| o.child_text(airsync::Class).is_none_or(|c| c != "SMS"))?;
        Some(Self {
            filter_type: parse(options.child_text(airsync::FilterType)).unwrap_or(0),
            body_preferences: body_preferences(options),
            supported: None,
        })
    }

    /// The `<Supported>` fields of a collection.
    fn supported(collection: &Element) -> Option<Vec<String>> {
        let supported = collection.find(airsync::Supported)?;
        Some(supported.elements().map(|e| e.tag.to_string()).collect())
    }

    fn stored(state: Option<&CollectionSyncState>) -> Option<Self> {
        serde_json::from_str(state?.options.as_deref()?).ok()
    }
}

/// The `<airsyncbase:BodyPreference>` entries of an `<Options>` element.
pub(super) fn body_preferences(options: &Element) -> Vec<BodyPreference> {
    options
        .find_all(airsyncbase::BodyPreference)
        .filter_map(|p| {
            Some(BodyPreference {
                body_type: parse(p.child_text(airsyncbase::Type))?,
                truncation_size: parse(p.child_text(airsyncbase::TruncationSize)),
                all_or_none: p.child_text(airsyncbase::AllOrNone) == Some("1"),
                preview: parse(p.child_text(airsyncbase::Preview)),
            })
        })
        .collect()
}

fn parse<T: std::str::FromStr>(value: Option<&str>) -> Option<T> {
    value?.trim().parse().ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FolderKind {
    Mail,
    Contacts,
    Calendar,
}

pub(super) struct Folder {
    pub id: u32,
    pub kind: FolderKind,
    /// The JMAP mailbox, address book or calendar id.
    pub jmap_id: String,
    /// The account of a shared mailbox, address book or calendar; `None`
    /// for the user's own (primary) account.
    pub account: Option<String>,
}

/// How a mailbox, address book or calendar id is stored after its prefix
/// (mailboxes have none): `<id>` in the primary account, `<account>/<id>`
/// in a shared one (JMAP ids cannot contain `/`).
pub(super) fn split_account(stored: &str) -> (Option<&str>, &str) {
    match stored.split_once('/') {
        Some((account, id)) => (Some(account), id),
        None => (None, stored),
    }
}

/// The stored form of an address book or calendar id (see [`split_account`]).
pub(super) fn stored_id(prefix: &str, account: Option<&str>, id: &str) -> String {
    match account {
        Some(account) => format!("{prefix}{account}/{id}"),
        None => format!("{prefix}{id}"),
    }
}

pub(super) fn resolve(req: &Request, collection_id: &str) -> Result<Option<Folder>, CommandError> {
    let Ok(id) = collection_id.parse() else {
        return Ok(None);
    };
    Ok(req.store.mailbox_for_folder(req.user, id)?.map(|stored| {
        let collection = |kind, rest: &str| {
            let (account, jmap_id) = split_account(rest);
            Folder {
                id,
                kind,
                jmap_id: jmap_id.to_owned(),
                account: account.map(str::to_owned),
            }
        };
        if let Some(address_book) = stored.strip_prefix(contacts::PREFIX) {
            collection(FolderKind::Contacts, address_book)
        } else if let Some(calendar) = stored.strip_prefix(calendar::PREFIX) {
            collection(FolderKind::Calendar, calendar)
        } else {
            collection(FolderKind::Mail, &stored)
        }
    }))
}

/// The folder's current items with their versions.
async fn current_items(
    req: &Request<'_>,
    jmap: &JmapClient,
    collection_id: &str,
    folder: &Folder,
    options: &SyncOptions,
) -> anyhow::Result<Vec<(String, u64)>> {
    let jmap = &*folder.client(jmap);
    let days = view::window_days(options.filter_type, req.caps.sync_max_age_days);
    match folder.kind {
        FolderKind::Mail => {
            let source = view::MailSource::new(jmap, &folder.jmap_id, days);
            view::current_items(req, collection_id, &source).await
        }
        FolderKind::Contacts => {
            let source = contacts::ContactsSource {
                jmap,
                address_book: &folder.jmap_id,
                max_picture: req.caps.contact_picture_max_bytes,
            };
            view::current_items(req, collection_id, &source).await
        }
        FolderKind::Calendar => {
            let body = BodyPreference::choose(&options.body_preferences);
            let source = calendar::CalendarSource {
                jmap,
                calendar: &folder.jmap_id,
                days,
                body: &body,
                own: req.own,
            };
            view::current_items(req, collection_id, &source).await
        }
    }
}

impl Folder {
    /// The client for the folder's account.
    pub fn client<'a>(&self, jmap: &'a JmapClient) -> std::borrow::Cow<'a, JmapClient> {
        match &self.account {
            Some(account) => std::borrow::Cow::Owned(jmap.in_account(account)),
            None => std::borrow::Cow::Borrowed(jmap),
        }
    }

    pub fn server_id(&self, item_id: u32) -> String {
        format!("{}:{item_id}", self.id)
    }

    /// The email or card behind a `folder:item` ServerId of this folder.
    pub fn email_for(&self, req: &Request, server_id: &str) -> anyhow::Result<Option<String>> {
        let Some((folder, item)) = server_id.split_once(':') else {
            return Ok(None);
        };
        match (folder.parse::<u32>(), item.parse()) {
            (Ok(folder), Ok(item)) if folder == self.id => {
                req.store.email_for_item(req.user, folder, item)
            }
            _ => Ok(None),
        }
    }
}

/// What the device must be told to get from `base` to the current state.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    deletes: Vec<String>,
    changes: Vec<(String, u64)>,
    /// Newest first.
    adds: Vec<String>,
}

impl Plan {
    fn new(base: &Snapshot, current: &[(String, u64)]) -> Self {
        let mut plan = Plan::default();
        for (id, flags) in current {
            match base.get(id) {
                None => plan.adds.push(id.clone()),
                Some(old) if old != flags => plan.changes.push((id.clone(), *flags)),
                Some(_) => {}
            }
        }
        let present: HashSet<&str> = current.iter().map(|(id, _)| id.as_str()).collect();
        plan.deletes = base
            .keys()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        plan
    }

    fn len(&self) -> usize {
        self.deletes.len() + self.changes.len() + self.adds.len()
    }

    /// The ids of every operation, in the order they are sent.
    fn ids(&self) -> impl Iterator<Item = &str> {
        self.deletes
            .iter()
            .chain(self.changes.iter().map(|(id, _)| id))
            .chain(&self.adds)
            .map(String::as_str)
    }

    fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.deletes.retain(|id| keep(id));
        self.changes.retain(|(id, _)| keep(id));
        self.adds.retain(|id| keep(id));
    }

    /// Keeps the first `window` operations (deletes, flag changes, then adds)
    /// and returns whether anything was left out.
    fn truncate(&mut self, window: usize) -> bool {
        let more = self.len() > window;
        let mut left = window;
        self.deletes.truncate(left);
        left -= self.deletes.len();
        self.changes.truncate(left);
        left -= self.changes.len();
        self.adds.truncate(left);
        more
    }
}

/// The snapshot the device's SyncKey stands for, with its stored JSON.
fn base_snapshot(
    state: Option<&CollectionSyncState>,
    key: u32,
) -> Result<Option<(Snapshot, String)>, CommandError> {
    let json = match state {
        Some(s) if s.sync_key == key => &s.snapshot,
        Some(s) => match &s.previous {
            Some((prev_key, snapshot)) if *prev_key == key => snapshot,
            _ => return Ok(None),
        },
        None => return Ok(None),
    };
    let snapshot = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    Ok(Some((snapshot, json.clone())))
}

fn to_json(snapshot: &Snapshot) -> anyhow::Result<String> {
    Ok(serde_json::to_string(snapshot)?)
}

/// Answers a Sync; `None` is an empty response (a wait that ran out).
pub async fn handle(
    req: &Request<'_>,
    jmap: &JmapClient,
    push: &PushHub,
) -> Result<Option<Element>, CommandError> {
    let request = match full_request(req)? {
        Ok(request) => request,
        Err(reply) => return Ok(Some(reply)),
    };
    let Some(collections) = request.find(airsync::Collections) else {
        return Ok(Some(sync_status(status::PROTOCOL_ERROR)));
    };
    let window = parse(request.child_text(airsync::WindowSize));
    let wait = match wait_time(&request) {
        Ok(wait) => wait,
        Err(reply) => return Ok(Some(reply)),
    };
    let Some(wait) = wait else {
        return Ok(Some(sync_all(req, jmap, collections, window).await?));
    };

    // Subscribe before the first pass so no change slips in between.
    let account = push.account(jmap);
    let mut changes = account.subscribe();
    let mut next_request = push.device_requests(req.user, req.device_id);
    let mut shutdown = push.shutdown_signal();
    let deadline = tokio::time::Instant::now() + wait;
    // Push does not cover shared accounts (see `SHARED_POLL_SECS`).
    let poll = if has_shared(req, collections)? {
        req.caps.shared_poll.min(FALLBACK_POLL)
    } else {
        FALLBACK_POLL
    };
    loop {
        changes.borrow_and_update();
        let reply = sync_all(req, jmap, collections, window).await?;
        if has_news(&reply) {
            return Ok(Some(reply));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        tokio::select! {
            result = changes.changed() => {
                if result.is_err() {
                    tokio::time::sleep(remaining.min(poll)).await;
                }
            }
            // The device sent another request, or we are stopping.
            _ = next_request.changed() => return Ok(None),
            _ = shutdown.changed() => return Ok(None),
            _ = tokio::time::sleep(remaining.min(poll)) => {}
        }
    }
}

/// Whether any of the request's collections is in a shared account.
fn has_shared(req: &Request, collections: &Element) -> Result<bool, CommandError> {
    for collection in collections.find_all(airsync::Collection) {
        if let Some(id) = collection.child_text(airsync::CollectionId)
            && resolve(req, id)?.is_some_and(|folder| folder.account.is_some())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Re-checks waiting Syncs at least this often in case push is down.
const FALLBACK_POLL: Duration = Duration::from_secs(120);

/// Every collection of the request, each failing on its own.
async fn sync_all(
    req: &Request<'_>,
    jmap: &JmapClient,
    collections: &Element,
    window: Option<usize>,
) -> CommandResult {
    let mut out = Element::new(airsync::Collections);
    for collection in collections.find_all(airsync::Collection) {
        let reply = match sync_collection(req, jmap, collection, window).await {
            Ok(reply) => reply,
            // One failing collection must not fail the others (or the whole
            // request: clients back off for a long time after an HTTP 500).
            Err(CommandError::Backend(e)) if !jmap::is_unauthorized(&e) => {
                let collection_id = collection
                    .child_text(airsync::CollectionId)
                    .unwrap_or_default();
                tracing::warn!(
                    collection_id,
                    error = format!("{e:#}"),
                    "collection sync failed"
                );
                req.store.note_device_error(
                    req.user,
                    req.device_id,
                    &format!("Sync of collection {collection_id}: {e:#}"),
                )?;
                Element::new(airsync::Collection)
                    .child(Element::with_text(
                        airsync::SyncKey,
                        collection.child_text(airsync::SyncKey).unwrap_or("0"),
                    ))
                    .child(Element::with_text(airsync::CollectionId, collection_id))
                    .child(Element::with_text(airsync::Status, status::SERVER_ERROR))
            }
            Err(e) => return Err(e),
        };
        out.push(reply);
    }
    Ok(Element::new(airsync::Sync).child(out))
}

/// Whether a Sync response tells the device anything beyond "no changes".
fn has_news(reply: &Element) -> bool {
    reply
        .find(airsync::Collections)
        .into_iter()
        .flat_map(|c| c.find_all(airsync::Collection))
        .any(|c| {
            c.child_text(airsync::Status) != Some(status::OK)
                || c.find(airsync::Commands).is_some()
                || c.find(airsync::Responses).is_some()
                || c.find(airsync::MoreAvailable).is_some()
        })
}

fn sync_status(status: &str) -> Element {
    Element::new(airsync::Sync).child(Element::with_text(airsync::Status, status))
}

async fn sync_collection(
    req: &Request<'_>,
    jmap: &JmapClient,
    collection: &Element,
    global_window: Option<usize>,
) -> CommandResult {
    let collection_id = collection
        .child_text(airsync::CollectionId)
        .unwrap_or_default();
    let reply = |key: &str, status: &str| {
        Element::new(airsync::Collection)
            .child(Element::with_text(airsync::SyncKey, key))
            .child(Element::with_text(airsync::CollectionId, collection_id))
            .child(Element::with_text(airsync::Status, status))
    };
    let Some(key) = parse::<u32>(collection.child_text(airsync::SyncKey)) else {
        return Ok(reply("0", status::INVALID_SYNC_KEY));
    };
    let Some(folder) = resolve(req, collection_id)? else {
        return Ok(reply("0", status::HIERARCHY_CHANGED));
    };
    let jmap = &*folder.client(jmap);

    let state = req
        .store
        .collection_sync_state(req.user, req.device_id, collection_id)?;
    let stored_options = SyncOptions::stored(state.as_ref());
    let mut options = SyncOptions::from_collection(collection)
        .or_else(|| stored_options.clone())
        .unwrap_or_default();
    // Sent with SyncKey 0 only, and kept until the next one.
    options.supported = match SyncOptions::supported(collection) {
        Some(supported) => Some(supported),
        None if key == 0 => None,
        None => stored_options.as_ref().and_then(|o| o.supported.clone()),
    };
    let options_json = Some(serde_json::to_string(&options).map_err(anyhow::Error::from)?);
    let last_key = state.as_ref().map_or(0, |s| s.sync_key);
    let save = |state: CollectionSyncState| {
        req.store
            .save_collection_sync(req.user, req.device_id, collection_id, &state)
    };

    // Key 0 (re)starts the collection; the first response carries no items.
    if key == 0 {
        let new_key = last_key + 1;
        save(CollectionSyncState {
            sync_key: new_key,
            snapshot: "{}".to_owned(),
            previous: None,
            options: options_json,
        })?;
        return Ok(reply(&new_key.to_string(), status::OK));
    }
    let Some((base, base_json)) = base_snapshot(state.as_ref(), key)? else {
        return Ok(reply("0", status::INVALID_SYNC_KEY));
    };
    let mut repeats = LoopCheck::new(req, collection_id, key, state.as_ref())?;

    let body = BodyPreference::choose(&options.body_preferences);
    let mut snapshot = base.clone();
    let mut responses = Vec::new();
    // Contacts and events the device created or changed: their versions come from the
    // refreshed view below, so the change does not echo back.
    let mut touched = Vec::new();
    let client_changes = match collection.find(airsync::Commands) {
        Some(commands) => {
            match folder.kind {
                FolderKind::Mail => {
                    // DeletesAsMoves defaults to true when absent.
                    let deletes_as_moves = collection
                        .find(airsync::DeletesAsMoves)
                        .is_none_or(|d| d.text() != Some("0"));
                    let ctx = ClientCommands {
                        req,
                        jmap,
                        folder: &folder,
                        body: &body,
                        deletes_as_moves,
                    };
                    responses = ctx.apply(commands, &mut snapshot).await?;
                }
                FolderKind::Contacts => {
                    let mut kind = contacts::ContactChanges::new(
                        jmap,
                        &folder.jmap_id,
                        options.supported.as_deref(),
                        req.caps.contact_picture_max_bytes,
                    );
                    let applied =
                        device_changes::apply(req, &folder, commands, &mut kind, &mut snapshot)
                            .await?;
                    responses = applied.responses;
                    touched = applied.touched;
                }
                FolderKind::Calendar => {
                    let mut kind =
                        calendar::CalendarChanges::new(req, jmap, &folder.jmap_id, &body);
                    let applied =
                        device_changes::apply(req, &folder, commands, &mut kind, &mut snapshot)
                            .await?;
                    responses = applied.responses;
                    touched = applied.touched;
                }
            }
            true
        }
        None => false,
    };

    let get_changes = collection
        .find(airsync::GetChanges)
        .is_none_or(|g| g.text() != Some("0"));
    let current = if get_changes || !touched.is_empty() {
        current_items(req, jmap, collection_id, &folder, &options).await?
    } else {
        Vec::new()
    };
    for id in touched {
        if let Some((_, version)) = current.iter().find(|(c, _)| *c == id) {
            snapshot.insert(id, *version);
        }
    }
    if get_changes {
        skip_quarantined(&mut snapshot, &current, &repeats.quarantined);
    }
    let mut plan = if get_changes {
        Plan::new(&snapshot, &current)
    } else {
        Plan::default()
    };

    if plan.len() == 0 && !client_changes {
        repeats.caught_up(req, collection_id)?;
        if stored_options.as_ref() != Some(&options)
            && let Some(mut state) = state
        {
            state.options = options_json;
            save(state)?;
        }
        return Ok(reply(&key.to_string(), status::OK));
    }

    let mut window = parse(collection.child_text(airsync::WindowSize))
        .or(global_window)
        .unwrap_or(DEFAULT_WINDOW)
        .clamp(1, MAX_WINDOW);
    let planned = plan.len();
    // Probing: only the suspects, one at a time; the rest waits.
    if repeats.probe(&plan, window) {
        let suspects = &repeats.state.suspects;
        plan.retain(|id| suspects.iter().any(|s| s == id));
        window = 1;
    }
    let more_available = plan.truncate(window) || plan.len() < planned;
    let sent: Vec<String> = plan.ids().map(str::to_owned).collect();

    let mut commands = Element::new(airsync::Commands);
    let adds = match folder.kind {
        FolderKind::Mail => {
            mail_commands(
                req,
                jmap,
                &folder,
                &plan,
                &body,
                &mut snapshot,
                &mut commands,
            )
            .await?
        }
        FolderKind::Contacts | FolderKind::Calendar => {
            let wanted: Vec<&str> = plan
                .changes
                .iter()
                .map(|(id, _)| id.as_str())
                .chain(plan.adds.iter().map(String::as_str))
                .collect();
            let items = match folder.kind {
                FolderKind::Contacts => {
                    contacts::fetch(jmap, &wanted, req.caps.contact_picture_max_bytes).await?
                }
                _ => calendar::fetch(jmap, &wanted, &body, req.own).await?,
            };
            versioned_commands(req, &folder, &plan, items, &mut snapshot, &mut commands)?
        }
    };

    // Planned adds that could not be fetched are gone, though the view did
    // not hear of it (a server that misses a destroy in its changes):
    // forget them, or every Sync and Ping would plan them again.
    let missing: Vec<&str> = plan
        .adds
        .iter()
        .filter(|id| !snapshot.contains_key(*id))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        tracing::debug!(collection_id, ?missing, "planned items are gone");
        view::forget(req, collection_id, &missing)?;
    }

    let new_key = last_key.max(key) + 1;
    save(CollectionSyncState {
        sync_key: new_key,
        snapshot: to_json(&snapshot)?,
        // The stored JSON as it was: no need to encode the base again.
        previous: Some((key, base_json)),
        options: options_json,
    })?;
    repeats.answered(req, collection_id, key, sent)?;
    tracing::debug!(
        collection_id,
        key,
        new_key,
        client_changes,
        deletes = plan.deletes.len(),
        changes = plan.changes.len(),
        adds,
        more_available,
        "sync"
    );

    let mut response = reply(&new_key.to_string(), status::OK);
    if !responses.is_empty() {
        response.push(
            responses
                .into_iter()
                .fold(Element::new(airsync::Responses), Element::child),
        );
    }
    if more_available {
        response.push(Element::new(airsync::MoreAvailable));
    }
    if repeats.state.probing && !commands.children.is_empty() {
        // What the device keeps rejecting, without the mail's content.
        tracing::info!(collection_id, shape = commands.shape(), "probe");
    }
    if !commands.children.is_empty() {
        response.push(commands);
    }
    Ok(response)
}

/// Loop detection for one collection request (see [`LOOP_REPEATS`]).
struct LoopCheck {
    state: SyncRepeats,
    /// Whether a row is stored.
    stored: bool,
    /// Whether the request repeats the key of the last response.
    retry: bool,
    /// The collection's quarantined items, for this device.
    quarantined: Vec<String>,
    /// Whether this request starts probing.
    starting: bool,
}

impl LoopCheck {
    /// Counts a repeated key; probes or quarantines when it keeps coming.
    fn new(
        req: &Request,
        collection_id: &str,
        key: u32,
        state: Option<&CollectionSyncState>,
    ) -> anyhow::Result<Self> {
        let (user, device) = (req.user, req.device_id);
        let stored = req.store.sync_repeats(user, device, collection_id)?;
        let mut check = Self {
            stored: stored.is_some(),
            state: stored.unwrap_or_default(),
            // The base was found, so another key means the previous one.
            retry: state.is_some_and(|s| s.sync_key != key),
            quarantined: req.store.quarantined(user, device, collection_id)?,
            starting: false,
        };
        let now = chrono::Utc::now().timestamp();
        let state = &mut check.state;
        let same = check.stored && state.sync_key == key && now - state.last_at < LOOP_GAP;
        state.repeats = match (check.retry, same) {
            (false, _) => 0,
            (true, true) => state.repeats + 1,
            (true, false) => 1,
        };
        if state.repeats >= LOOP_REPEATS && state.probing && state.sent.len() == 1 {
            let id = state.sent.remove(0);
            tracing::warn!(
                collection_id,
                object = id,
                "device keeps failing on this item; it is skipped for this device"
            );
            req.store.quarantine(user, device, collection_id, &id)?;
            check.quarantined.push(id);
            state.suspects.clear();
            state.probing = false;
            state.repeats = 0;
        } else if state.repeats >= LOOP_REPEATS && !state.probing {
            tracing::warn!(
                collection_id,
                sync_key = key,
                repeats = state.repeats,
                "sync loop: sending one item at a time to find the one the device fails on"
            );
            state.probing = true;
            state.repeats = 0;
            check.starting = true;
        }
        Ok(check)
    }

    /// Whether this response is a probe: with the plan the device keeps
    /// rejecting (the first `window` items), the suspects are recorded; once
    /// none is left to send, probing ends.
    fn probe(&mut self, plan: &Plan, window: usize) -> bool {
        let state = &mut self.state;
        if !state.probing {
            return false;
        }
        if self.starting {
            state.suspects = plan.ids().take(window).map(str::to_owned).collect();
        }
        if !plan.ids().any(|id| state.suspects.iter().any(|s| s == id)) {
            state.probing = false;
            state.suspects.clear();
            return false;
        }
        true
    }

    /// A response with a new key went out with these items.
    fn answered(
        &mut self,
        req: &Request,
        collection_id: &str,
        key: u32,
        sent: Vec<String>,
    ) -> anyhow::Result<()> {
        let state = &mut self.state;
        if state.repeats == 0 && !state.probing {
            return self.forget(req, collection_id);
        }
        state.sync_key = key;
        state.last_at = chrono::Utc::now().timestamp();
        state.sent = if state.probing { sent } else { Vec::new() };
        self.stored = true;
        req.store
            .save_sync_repeats(req.user, req.device_id, collection_id, state)
    }

    /// Nothing left to send: the device took everything.
    fn caught_up(&mut self, req: &Request, collection_id: &str) -> anyhow::Result<()> {
        if self.retry {
            return Ok(());
        }
        self.forget(req, collection_id)
    }

    fn forget(&mut self, req: &Request, collection_id: &str) -> anyhow::Result<()> {
        if std::mem::take(&mut self.stored) {
            req.store
                .delete_sync_repeats(req.user, req.device_id, collection_id)?;
        }
        Ok(())
    }
}

/// Treats quarantined items as sent: the device gets neither them nor their
/// changes or removal.
fn skip_quarantined(snapshot: &mut Snapshot, current: &[(String, u64)], quarantined: &[String]) {
    for id in quarantined {
        match current.iter().find(|(c, _)| c == id) {
            Some((_, version)) => {
                snapshot.insert(id.clone(), *version);
            }
            None => {
                snapshot.remove(id);
            }
        }
    }
}

/// Deletes plus the fetched changed and new items (contacts, events);
/// returns how many were added.
fn versioned_commands(
    req: &Request<'_>,
    folder: &Folder,
    plan: &Plan,
    cards: Vec<(String, u64, Element)>,
    snapshot: &mut Snapshot,
    commands: &mut Element,
) -> anyhow::Result<usize> {
    let referenced: Vec<&str> = plan
        .deletes
        .iter()
        .map(String::as_str)
        .chain(cards.iter().map(|(id, _, _)| id.as_str()))
        .collect();
    let item_ids = req.store.item_ids(req.user, folder.id, &referenced)?;
    let server_id =
        |id: &str| Element::with_text(airsync::ServerId, folder.server_id(item_ids[id]));

    for id in &plan.deletes {
        snapshot.remove(id);
        commands.push(Element::new(airsync::Delete).child(server_id(id)));
    }
    let mut adds = 0;
    for (id, version, data) in cards {
        let previous = snapshot.insert(id.clone(), version);
        let Some(previous) = previous else {
            adds += 1;
            commands.push(Element::new(airsync::Add).child(server_id(&id)).child(data));
            continue;
        };
        if folder.kind != FolderKind::Calendar {
            commands.push(
                Element::new(airsync::Change)
                    .child(server_id(&id))
                    .child(data),
            );
            continue;
        }
        // Calendar: event changes and response changes go separately, since
        // AOSP-based clients only refresh attendees in an attendees-only Change.
        let changed = previous ^ version;
        let attendees = calendar::attendees_only(&data);
        if changed & calendar::EVENT_HALF != 0 || attendees.is_none() {
            let mut data = data;
            calendar::stamp_now(&mut data);
            commands.push(
                Element::new(airsync::Change)
                    .child(server_id(&id))
                    .child(data),
            );
        }
        if changed & calendar::ATTENDEE_HALF != 0
            && let Some(attendees) = attendees
        {
            commands.push(
                Element::new(airsync::Change)
                    .child(server_id(&id))
                    .child(attendees),
            );
        }
    }
    Ok(adds)
}

/// Whether the server has changes the device has not synced yet; `None`
/// when the collection is unknown (the device must FolderSync first).
pub(super) async fn has_pending_changes(
    req: &Request<'_>,
    jmap: &JmapClient,
    collection_id: &str,
) -> Result<Option<bool>, CommandError> {
    let Some(folder) = resolve(req, collection_id)? else {
        return Ok(None);
    };
    let Some(state) = req
        .store
        .collection_sync_state(req.user, req.device_id, collection_id)?
    else {
        // Never synced: let the device start.
        return Ok(Some(true));
    };
    let mut snapshot: Snapshot =
        serde_json::from_str(&state.snapshot).map_err(anyhow::Error::from)?;
    let options = SyncOptions::stored(Some(&state)).unwrap_or_default();
    let current = current_items(req, jmap, collection_id, &folder, &options).await?;
    let quarantined = req
        .store
        .quarantined(req.user, req.device_id, collection_id)?;
    skip_quarantined(&mut snapshot, &current, &quarantined);
    Ok(Some(Plan::new(&snapshot, &current).len() > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn fixture(name: &str) -> Element {
        let path = format!(
            "{}/tests/fixtures/gmail-android/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        crate::wbxml::decode(&std::fs::read(path).unwrap()).unwrap()
    }

    fn first_collection(root: &Element) -> &Element {
        root.find(airsync::Collections)
            .and_then(|c| c.find(airsync::Collection))
            .unwrap()
    }

    #[test]
    fn replays_gmail_sync_requests() {
        let initial = fixture("14.1-sync-initial.req.wbxml");
        let collection = first_collection(&initial);
        assert_eq!(collection.child_text(airsync::SyncKey), Some("0"));
        assert_eq!(SyncOptions::from_collection(collection), None);

        let with_options = fixture("14.1-sync-key1-options.req.wbxml");
        let collection = first_collection(&with_options);
        let options = SyncOptions::from_collection(collection).unwrap();
        assert_eq!(options.filter_type, 3);
        assert_eq!(
            BodyPreference::choose(&options.body_preferences).body_type,
            2
        );
        assert_eq!(
            parse::<usize>(collection.child_text(airsync::WindowSize)),
            Some(50)
        );

        for (name, flagged) in [
            ("14.1-sync-change-flag-set.req.wbxml", true),
            ("14.1-sync-change-flag-clear.req.wbxml", false),
        ] {
            let root = fixture(name);
            let change = first_collection(&root)
                .find(airsync::Commands)
                .and_then(|c| c.find(airsync::Change))
                .unwrap();
            let flag = change
                .find(airsync::ApplicationData)
                .and_then(|d| d.find(email_cp::Flag))
                .unwrap();
            assert_eq!(
                flag.child_text(email_cp::Status) == Some("2"),
                flagged,
                "{name}"
            );
        }
    }

    fn items(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(id, f)| (id.to_string(), *f)).collect()
    }

    #[test]
    fn plans_adds_changes_and_deletes() {
        let base: Snapshot = [
            ("a".to_owned(), 0),
            ("b".to_owned(), READ),
            ("c".to_owned(), 0),
        ]
        .into();
        let current = items(&[
            ("new2", 0),
            ("new1", READ),
            ("a", READ | FLAGGED),
            ("b", READ),
        ]);
        let mut plan = Plan::new(&base, &current);
        assert_eq!(plan.adds, vec!["new2", "new1"]);
        assert_eq!(plan.changes, vec![("a".to_owned(), READ | FLAGGED)]);
        assert_eq!(plan.deletes, vec!["c"]);
        assert_eq!(plan.len(), 4);

        assert!(plan.truncate(3));
        assert_eq!(plan.adds, vec!["new2"]);
        assert!(!plan.truncate(3));
    }

    #[test]
    fn reads_sticky_options() {
        let collection = Element::new(airsync::Collection).child(
            Element::new(airsync::Options)
                .child(Element::with_text(airsync::FilterType, "3"))
                .child(
                    Element::new(airsyncbase::BodyPreference)
                        .child(Element::with_text(airsyncbase::Type, "2"))
                        .child(Element::with_text(airsyncbase::TruncationSize, "200000")),
                ),
        );
        let options = SyncOptions::from_collection(&collection).unwrap();
        assert_eq!(options.filter_type, 3);
        assert_eq!(
            options.body_preferences,
            vec![BodyPreference {
                body_type: 2,
                truncation_size: Some(200_000),
                all_or_none: false,
                preview: None,
            }]
        );
        assert_eq!(
            SyncOptions::from_collection(&Element::new(airsync::Collection)),
            None
        );
    }

    #[test]
    fn base_snapshot_accepts_current_and_previous_key() {
        let state = CollectionSyncState {
            sync_key: 5,
            snapshot: r#"{"b":1}"#.to_owned(),
            previous: Some((4, r#"{"a":0}"#.to_owned())),
            options: None,
        };
        assert_eq!(base_snapshot(Some(&state), 5).unwrap().unwrap().0["b"], 1);
        assert_eq!(base_snapshot(Some(&state), 4).unwrap().unwrap().0["a"], 0);
        assert!(base_snapshot(Some(&state), 3).unwrap().is_none());
        assert!(base_snapshot(None, 1).unwrap().is_none());
    }

    #[test]
    fn resolves_server_ids_within_folder() {
        let store = Store::open_in_memory().unwrap();
        let req = Request::for_tests(&store, None);
        let ids = store.item_ids("rob", 7, &["m1"]).unwrap();
        let folder = Folder {
            id: 7,
            kind: FolderKind::Mail,
            jmap_id: "mb".to_owned(),
            account: None,
        };
        let server_id = folder.server_id(ids["m1"]);
        assert_eq!(
            folder.email_for(&req, &server_id).unwrap().as_deref(),
            Some("m1")
        );
        assert_eq!(folder.email_for(&req, "8:1").unwrap(), None);
        assert_eq!(folder.email_for(&req, "garbage").unwrap(), None);
    }

    #[test]
    fn moves_update_both_snapshots() {
        let store = Store::open_in_memory().unwrap();
        let req = Request::for_tests(&store, None);
        let state = |snapshot: &str| CollectionSyncState {
            sync_key: 3,
            snapshot: snapshot.to_owned(),
            previous: Some((2, snapshot.to_owned())),
            options: None,
        };
        store
            .save_collection_sync("rob", "d1", "1", &state(r#"{"m":3}"#))
            .unwrap();
        store
            .save_collection_sync("rob", "d1", "2", &state("{}"))
            .unwrap();
        record_move(&req, "1", "2", "m", "m").unwrap();
        for collection in ["1", "2"] {
            let state = store
                .collection_sync_state("rob", "d1", collection)
                .unwrap()
                .unwrap();
            let expected = if collection == "1" {
                "{}"
            } else {
                r#"{"m":3}"#
            };
            assert_eq!(state.snapshot, expected, "collection {collection}");
            assert_eq!(
                state.previous.unwrap().1,
                expected,
                "previous of {collection}"
            );
        }
    }

    #[test]
    fn stores_shared_collections_with_their_account() {
        assert_eq!(stored_id("calendar:", None, "c1"), "calendar:c1");
        assert_eq!(stored_id("calendar:", Some("a2"), "c1"), "calendar:a2/c1");
        assert_eq!(split_account("a2/c1"), (Some("a2"), "c1"));
        assert_eq!(split_account("c1"), (None, "c1"));
    }

    #[test]
    fn move_patch_touches_only_the_two_mailboxes() {
        assert_eq!(
            move_patch("inbox", "trash"),
            json!({ "mailboxIds/inbox": null, "mailboxIds/trash": true })
        );
    }
}
