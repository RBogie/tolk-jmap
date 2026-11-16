//! The changes a device sends for contacts and calendars (Sync
//! `<Commands>`): Adds, Changes, Deletes and Fetches, applied to JMAP in one
//! `/set`. What differs per data type is a [`Kind`]; mail has its own
//! handling in `sync` (flags, moves and drafts).

use std::future::Future;

use serde_json::{Map, Value};

use super::{CommandError, Request, sync::Folder};
use crate::{
    jmap::SetResult,
    wbxml::{Element, Tag, cp::airsync},
};

pub(super) mod status {
    pub const OK: &str = "1";
    pub const SERVER_ERROR: &str = "5";
    pub const CONVERSION_ERROR: &str = "6";
    pub const CONFLICT: &str = "7";
    pub const OBJECT_NOT_FOUND: &str = "8";
}

/// What applying the device's commands produced.
pub(super) struct Applied {
    /// Elements for `<Responses>`.
    pub responses: Vec<Element>,
    /// Objects the device created or changed; their snapshot versions must
    /// be set from the refreshed view so the change does not echo back.
    pub touched: Vec<String>,
}

/// What a Change (or an EAS 16 occurrence Delete) does to an object.
pub(super) enum Patch {
    /// These patch entries.
    Apply(Map<String, Value>),
    /// Nothing (a Change without data).
    Skip,
    /// The command names something that does not exist (status 8).
    NotFound,
}

/// One kind of versioned collection, as the driver needs it.
pub(super) trait Kind {
    /// Called with the ids of every object the commands change, before any
    /// other method (calendar: loads those events).
    fn prepare(&mut self, _ids: &[&str]) -> impl Future<Output = anyhow::Result<()>> + Send {
        async { Ok(()) }
    }
    /// The JMAP object for a device Add.
    fn create(&mut self, data: &Element, client_id: &str) -> Map<String, Value>;
    /// What a Change, or a Delete that [`Kind::is_patch`] claims, does to `id`.
    fn patch(&mut self, id: &str, command: &Element) -> Patch;
    /// Whether this Delete changes the object instead of removing it.
    fn is_patch(&self, _command: &Element) -> bool {
        false
    }
    /// The version the device would be sent of `id` now, from what
    /// [`Kind::prepare`] loaded; `None` when unknown (no conflict check).
    fn current_version(&self, _id: &str) -> Option<u64> {
        None
    }
    /// The fields in the device's `ApplicationData` that have a value but no
    /// place on the JMAP server (see `UNSUPPORTED_FIELDS`).
    fn unsupported(&self, _data: &Element) -> Vec<&'static str> {
        Vec::new()
    }
    /// Whether the collection may be changed (a shared one may be read-only).
    fn writable(&self) -> impl Future<Output = anyhow::Result<bool>> + Send {
        async { Ok(true) }
    }
    fn set(
        &self,
        create: Map<String, Value>,
        update: Map<String, Value>,
        destroy: Vec<String>,
    ) -> impl Future<Output = anyhow::Result<SetResult>> + Send;
    /// Objects with their version and `ApplicationData`, for Fetch.
    fn fetch(
        &self,
        ids: &[&str],
    ) -> impl Future<Output = anyhow::Result<Vec<(String, u64, Element)>>> + Send;
}

enum Pending {
    Add {
        client_id: String,
        key: String,
    },
    Change {
        tag: Tag,
        server_id: String,
        id: String,
    },
    Delete {
        server_id: String,
        id: String,
    },
}

/// Applies the device's commands to JMAP and to its snapshot.
///
/// A Change of an object the server changed since the device was last sent
/// it is a conflict (MS-ASCMD 2.2.3.34): unless `client_wins` (the device's
/// `<Conflict>0</Conflict>`), the device's change is discarded with status 7
/// and the server's version goes to the device in the same response. A
/// Delete always wins.
pub(super) async fn apply(
    req: &Request<'_>,
    folder: &Folder,
    commands: &Element,
    kind: &mut (impl Kind + Send),
    snapshot: &mut super::sync::Snapshot,
    client_wins: bool,
    sync_key: u32,
) -> Result<Applied, CommandError> {
    let mut applied = Applied {
        responses: Vec::new(),
        touched: Vec::new(),
    };
    let mut changed = Vec::new();
    for command in commands
        .elements()
        .filter(|c| c.tag == airsync::Change || c.tag == airsync::Delete)
    {
        let server_id = command.child_text(airsync::ServerId).unwrap_or_default();
        if let Some(id) = folder.email_for(req, server_id)? {
            changed.push(id);
        }
    }
    kind.prepare(&changed.iter().map(String::as_str).collect::<Vec<_>>())
        .await?;
    let writes = commands
        .elements()
        .any(|c| [airsync::Add, airsync::Change, airsync::Delete].contains(&c.tag));
    let read_only = writes && !kind.writable().await?;

    let mut create = Map::new();
    let mut update: Map<String, Value> = Map::new();
    let mut destroy = Vec::new();
    let mut pending = Vec::new();
    let mut fetches = Vec::new();
    for (n, command) in commands.elements().enumerate() {
        if read_only && command.tag != airsync::Fetch {
            applied.responses.push(refuse(
                req,
                folder,
                command,
                kind.is_patch(command),
                snapshot,
            )?);
            continue;
        }
        if command.tag == airsync::Add {
            let client_id = command
                .child_text(airsync::ClientId)
                .unwrap_or_default()
                .to_owned();
            if let Some((id, response)) = repeated_add(req, folder, &client_id, sync_key)? {
                applied.responses.push(response);
                applied.touched.push(id);
                continue;
            }
            let Some(data) = command.find(airsync::ApplicationData) else {
                applied
                    .responses
                    .push(add_status(&client_id, None, status::CONVERSION_ERROR));
                continue;
            };
            if refuse_unsupported(req, kind, data) {
                applied
                    .responses
                    .push(add_status(&client_id, None, status::CONVERSION_ERROR));
                continue;
            }
            let key = format!("c{n}");
            create.insert(key.clone(), Value::Object(kind.create(data, &client_id)));
            pending.push(Pending::Add { client_id, key });
            continue;
        }
        let server_id = command
            .child_text(airsync::ServerId)
            .unwrap_or_default()
            .to_owned();
        let Some(id) = folder.email_for(req, &server_id)? else {
            applied.responses.push(item_status(
                command.tag,
                &server_id,
                status::OBJECT_NOT_FOUND,
            ));
            continue;
        };
        let is_change = command.tag == airsync::Change
            || (command.tag == airsync::Delete && kind.is_patch(command));
        if is_change {
            if let Some(data) = command.find(airsync::ApplicationData)
                && refuse_unsupported(req, kind, data)
            {
                applied.responses.push(item_status(
                    command.tag,
                    &server_id,
                    status::CONVERSION_ERROR,
                ));
                continue;
            }
            if !client_wins
                && let (Some(held), Some(now)) = (snapshot.get(&id), kind.current_version(&id))
                && *held != now
            {
                tracing::info!(
                    object = id,
                    "device change conflicts with a server change; the server's version wins"
                );
                applied
                    .responses
                    .push(item_status(command.tag, &server_id, status::CONFLICT));
                continue;
            }
            match kind.patch(&id, command) {
                Patch::Apply(entries) => {
                    // Several commands may change one object (occurrences of a series).
                    if let Value::Object(patch) = update
                        .entry(id.clone())
                        .or_insert_with(|| Value::Object(Map::new()))
                    {
                        patch.extend(entries);
                    }
                    pending.push(Pending::Change {
                        tag: command.tag,
                        server_id,
                        id,
                    });
                }
                Patch::Skip => {}
                Patch::NotFound => {
                    applied.responses.push(item_status(
                        command.tag,
                        &server_id,
                        status::OBJECT_NOT_FOUND,
                    ));
                }
            }
            continue;
        }
        match command.tag {
            airsync::Delete => {
                destroy.push(id.clone());
                pending.push(Pending::Delete { server_id, id });
            }
            airsync::Fetch => fetches.push((server_id, id)),
            _ => {}
        }
    }

    let result = kind.set(create, update, destroy).await?;
    for p in pending {
        match p {
            Pending::Add { client_id, key } => {
                let Some(id) = result.created.get(&key).and_then(|c| c["id"].as_str()) else {
                    tracing::warn!(error = ?result.not_created.get(&key), "device Add rejected");
                    applied
                        .responses
                        .push(add_status(&client_id, None, status::SERVER_ERROR));
                    continue;
                };
                record_add(req, folder, &client_id, id, sync_key)?;
                let item = req.store.item_ids(req.user, folder.id, &[id])?[id];
                applied.responses.push(add_status(
                    &client_id,
                    Some(&folder.server_id(item)),
                    status::OK,
                ));
                applied.touched.push(id.to_owned());
            }
            Pending::Change { tag, server_id, id } => match result.error(&id) {
                Some(error) => {
                    tracing::warn!(object = id, error, "device Change rejected");
                    applied
                        .responses
                        .push(item_status(tag, &server_id, failure(error)));
                }
                None => applied.touched.push(id),
            },
            Pending::Delete { server_id, id } => match result.error(&id) {
                Some(error) if error != "notFound" => {
                    applied.responses.push(item_status(
                        airsync::Delete,
                        &server_id,
                        failure(error),
                    ));
                }
                _ => {
                    snapshot.remove(&id);
                }
            },
        }
    }

    if !fetches.is_empty() {
        let ids: Vec<&str> = fetches.iter().map(|(_, id)| id.as_str()).collect();
        let found = kind.fetch(&ids).await?;
        for (server_id, id) in &fetches {
            applied
                .responses
                .push(match found.iter().find(|(f, _, _)| f == id) {
                    Some((_, _, data)) => {
                        item_status(airsync::Fetch, server_id, status::OK).child(data.clone())
                    }
                    None => item_status(airsync::Fetch, server_id, status::OBJECT_NOT_FOUND),
                });
        }
    }
    Ok(applied)
}

/// Logs the fields of `data` the server cannot store; whether the command
/// is refused for them (`UNSUPPORTED_FIELDS=reject`).
fn refuse_unsupported(req: &Request, kind: &impl Kind, data: &Element) -> bool {
    let fields = kind.unsupported(data);
    if fields.is_empty() {
        return false;
    }
    let reject = req.caps.unsupported_fields == crate::config::UnsupportedFields::Reject;
    tracing::info!(
        ?fields,
        rejected = reject,
        "device sent fields the JMAP server cannot store"
    );
    reject
}

/// The answer to a change in a read-only collection. The device is sent the
/// server's version again: a changed object (or `patch`, an occurrence
/// Delete) as a Change, a deleted one as an Add.
fn refuse(
    req: &Request,
    folder: &Folder,
    command: &Element,
    patch: bool,
    snapshot: &mut super::sync::Snapshot,
) -> anyhow::Result<Element> {
    if command.tag == airsync::Add {
        let client_id = command.child_text(airsync::ClientId).unwrap_or_default();
        tracing::info!(client_id, "device Add in a read-only collection refused");
        return Ok(add_status(client_id, None, status::CONVERSION_ERROR));
    }
    let server_id = command.child_text(airsync::ServerId).unwrap_or_default();
    let Some(id) = folder.email_for(req, server_id)? else {
        return Ok(item_status(
            command.tag,
            server_id,
            status::OBJECT_NOT_FOUND,
        ));
    };
    tracing::info!(
        object = id,
        "device change in a read-only collection refused"
    );
    if command.tag == airsync::Delete && !patch {
        if snapshot.remove(&id).is_none() {
            return Ok(item_status(
                command.tag,
                server_id,
                status::OBJECT_NOT_FOUND,
            ));
        }
    } else {
        // No real version is 0, so the next Sync sends it again.
        snapshot.insert(id, 0);
    }
    Ok(item_status(command.tag, server_id, status::CONFLICT))
}

/// The `<Responses>` entry for an Add this device already made with the same
/// ClientId in a Sync with the same SyncKey (it retried a Sync whose response
/// got lost), and the object it created; `None` for a new Add. A ClientId is
/// only unique within one Sync (MS-ASCMD 2.2.3.28.2), so a later Sync may
/// reuse it for a new object.
pub(super) fn repeated_add(
    req: &Request,
    folder: &Folder,
    client_id: &str,
    sync_key: u32,
) -> anyhow::Result<Option<(String, Element)>> {
    if client_id.is_empty() {
        return Ok(None);
    }
    let Some(id) = req
        .store
        .client_add(req.user, req.device_id, folder.id, client_id, sync_key)?
    else {
        return Ok(None);
    };
    tracing::info!(client_id, object = id, "repeated Add, not created again");
    let item = req.store.item_ids(req.user, folder.id, &[&id])?[&id];
    let response = add_status(client_id, Some(&folder.server_id(item)), status::OK);
    Ok(Some((id, response)))
}

/// Remembers what an Add created, for [`repeated_add`].
pub(super) fn record_add(
    req: &Request,
    folder: &Folder,
    client_id: &str,
    id: &str,
    sync_key: u32,
) -> anyhow::Result<()> {
    if client_id.is_empty() {
        return Ok(());
    }
    req.store
        .record_client_add(req.user, req.device_id, folder.id, client_id, id, sync_key)
}

pub(super) fn failure(error: &str) -> &'static str {
    if error == "notFound" {
        status::OBJECT_NOT_FOUND
    } else {
        status::SERVER_ERROR
    }
}

pub(super) fn add_status(client_id: &str, server_id: Option<&str>, status: &str) -> Element {
    let mut el = Element::new(airsync::Add).child(Element::with_text(airsync::ClientId, client_id));
    if let Some(server_id) = server_id {
        el.push(Element::with_text(airsync::ServerId, server_id));
    }
    el.child(Element::with_text(airsync::Status, status))
}

pub(super) fn item_status(tag: Tag, server_id: &str, status: &str) -> Element {
    Element::new(tag)
        .child(Element::with_text(airsync::ServerId, server_id))
        .child(Element::with_text(airsync::Status, status))
}
