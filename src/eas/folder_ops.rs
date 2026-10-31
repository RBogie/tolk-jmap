//! FolderCreate, FolderUpdate and FolderDelete (MS-ASCMD 2.2.1.2 to 2.2.1.4).
//!
//! Mail folders are JMAP mailboxes. A deleted mail folder moves into Trash,
//! like Exchange moves it into Deleted Items; one already in Trash is
//! destroyed with its messages. Address books and calendars can be created
//! and renamed, not deleted, and special folders cannot be changed, nor
//! can mailboxes, address books and calendars other users share.
//!
//! The device's hierarchy snapshot advances by exactly its own change, so
//! anything else that changed meanwhile still reaches it by FolderSync.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::{
    CommandError, CommandResult, Request, calendar, contacts,
    folder_sync::{self, Folder, folder_type},
};
use crate::{
    jmap::{DataType, JmapClient, Mailbox, SetResult},
    wbxml::{Element, Tag, cp::folder_hierarchy as fh},
};

mod status {
    pub const OK: &str = "1";
    pub const EXISTS: &str = "2";
    pub const SPECIAL_FOLDER: &str = "3";
    pub const NOT_FOUND: &str = "4";
    pub const PARENT_NOT_FOUND: &str = "5";
    pub const SERVER_ERROR: &str = "6";
    pub const INVALID_SYNC_KEY: &str = "9";
    pub const MALFORMED: &str = "10";
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Create,
    Update,
    Delete,
}

impl Operation {
    pub fn from_command(cmd: &str) -> Option<Self> {
        match cmd {
            "FolderCreate" => Some(Self::Create),
            "FolderUpdate" => Some(Self::Update),
            "FolderDelete" => Some(Self::Delete),
            _ => None,
        }
    }

    pub fn root(self) -> Tag {
        match self {
            Self::Create => fh::FolderCreate,
            Self::Update => fh::FolderUpdate,
            Self::Delete => fh::FolderDelete,
        }
    }
}

/// What a folder's stored id stands for.
enum Target {
    Mailbox(String),
    Collection { data_type: DataType, id: String },
}

fn target(stored: &str) -> Target {
    if let Some(id) = stored.strip_prefix(contacts::PREFIX) {
        Target::Collection {
            data_type: crate::jmap::ADDRESS_BOOK,
            id: id.to_owned(),
        }
    } else if let Some(id) = stored.strip_prefix(calendar::PREFIX) {
        Target::Collection {
            data_type: crate::jmap::CALENDAR,
            id: id.to_owned(),
        }
    } else {
        Target::Mailbox(stored.to_owned())
    }
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient, op: Operation) -> CommandResult {
    let body = req.body_with_root(op.root())?;
    let reply =
        |status: &str| Element::new(op.root()).child(Element::with_text(fh::Status, status));
    let Some(key) = body
        .child_text(fh::SyncKey)
        .and_then(|k| k.trim().parse::<u32>().ok())
    else {
        return Ok(reply(status::MALFORMED));
    };
    let Some(state) = req.store.folder_sync_state(req.user, req.device_id)? else {
        return Ok(reply(status::INVALID_SYNC_KEY));
    };
    if state.sync_key != key {
        return Ok(reply(status::INVALID_SYNC_KEY));
    }
    let known = folder_sync::parse_snapshot(&state.snapshot)?;

    let outcome = match op {
        Operation::Create => create(req, jmap, body).await?,
        Operation::Update => update(req, jmap, body).await?,
        Operation::Delete => delete(req, jmap, body).await?,
    };
    let changed = match outcome {
        Ok(changed) => changed,
        Err(status) => return Ok(reply(status)),
    };

    // The device now knows about its own change, and only that.
    let current = folder_sync::current_folders(req, jmap).await?;
    let mut snapshot: Vec<Folder> = known.clone();
    match op {
        Operation::Delete => {
            let gone = subtree(&known, &changed);
            snapshot.retain(|f| !gone.contains(&f.server_id));
        }
        _ => {
            let Some(folder) = current.iter().find(|f| f.server_id == changed) else {
                return Ok(reply(status::SERVER_ERROR));
            };
            match snapshot.iter_mut().find(|f| f.server_id == changed) {
                Some(existing) => *existing = folder.clone(),
                None => snapshot.push(folder.clone()),
            }
        }
    }
    let new_key = key + 1;
    req.store.save_folder_sync(
        req.user,
        req.device_id,
        Some((key, state.snapshot.as_str())),
        new_key,
        &serde_json::to_string(&snapshot).map_err(anyhow::Error::from)?,
    )?;
    let mut response =
        reply(status::OK).child(Element::with_text(fh::SyncKey, new_key.to_string()));
    if op == Operation::Create {
        response.push(Element::with_text(fh::ServerId, changed));
    }
    Ok(response)
}

/// The folder and everything below it, by server id.
fn subtree(folders: &[Folder], root: &str) -> HashSet<String> {
    let mut found = HashSet::from([root.to_owned()]);
    loop {
        let before = found.len();
        for folder in folders {
            if found.contains(&folder.parent_id) {
                found.insert(folder.server_id.clone());
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// The server id of the changed folder, or the failure status.
type Outcome = Result<String, &'static str>;

async fn create(
    req: &Request<'_>,
    jmap: &JmapClient,
    body: &Element,
) -> Result<Outcome, CommandError> {
    let (Some(name), Some(kind)) = (
        body.child_text(fh::DisplayName)
            .map(str::trim)
            .filter(|n| !n.is_empty()),
        body.child_text(fh::Type)
            .and_then(|t| t.trim().parse::<u8>().ok()),
    ) else {
        return Ok(Err(status::MALFORMED));
    };
    let parent = body.child_text(fh::ParentId).unwrap_or("0");
    let mut object = Map::new();
    object.insert("name".to_owned(), name.into());
    let (data_type, prefix) = match kind {
        folder_type::USER_CALENDAR => (crate::jmap::CALENDAR, calendar::PREFIX),
        folder_type::USER_CONTACTS => (crate::jmap::ADDRESS_BOOK, contacts::PREFIX),
        1 | folder_type::USER_MAIL => (crate::jmap::MAILBOX, ""),
        _ => return Ok(Err(status::MALFORMED)),
    };
    if data_type.name == crate::jmap::MAILBOX.name {
        let parent_id = match parent_mailbox(req, parent)? {
            Ok(parent_id) => parent_id,
            Err(status) => return Ok(Err(status)),
        };
        let mailboxes = jmap.mailboxes().await?;
        if mailboxes
            .iter()
            .any(|m| m.parent_id == parent_id && m.name.eq_ignore_ascii_case(name))
        {
            return Ok(Err(status::EXISTS));
        }
        object.insert("parentId".to_owned(), json!(parent_id));
    } else if parent != "0" {
        return Ok(Err(status::PARENT_NOT_FOUND));
    }
    let mut create = Map::new();
    create.insert("f".to_owned(), Value::Object(object));
    let result = jmap
        .set(data_type, crate::jmap::SetRequest::default().create(create))
        .await?;
    let Some(id) = result.created.get("f").and_then(|c| c["id"].as_str()) else {
        tracing::warn!(error = ?result.not_created.get("f"), "folder not created");
        return Ok(Err(status::SERVER_ERROR));
    };
    let stored = format!("{prefix}{id}");
    let server_id = req.store.folder_server_ids(req.user, &[&stored])?[&stored];
    tracing::info!(name, data_type = data_type.name, "device created a folder");
    Ok(Ok(server_id.to_string()))
}

async fn update(
    req: &Request<'_>,
    jmap: &JmapClient,
    body: &Element,
) -> Result<Outcome, CommandError> {
    let (Some(server_id), Some(name)) = (
        body.child_text(fh::ServerId),
        body.child_text(fh::DisplayName)
            .map(str::trim)
            .filter(|n| !n.is_empty()),
    ) else {
        return Ok(Err(status::MALFORMED));
    };
    let Some(stored) = stored_folder(req, server_id)? else {
        return Ok(Err(status::NOT_FOUND));
    };
    let result = match target(&stored) {
        // Someone else's: not renamed from this user's phone.
        Target::Mailbox(id) if id.contains('/') => return Ok(Err(status::SPECIAL_FOLDER)),
        Target::Mailbox(id) => {
            let mailboxes = jmap.mailboxes().await?;
            let Some(mailbox) = mailboxes.iter().find(|m| m.id == id) else {
                return Ok(Err(status::NOT_FOUND));
            };
            if mailbox.role.is_some() {
                return Ok(Err(status::SPECIAL_FOLDER));
            }
            let parent_id = match parent_mailbox(req, body.child_text(fh::ParentId).unwrap_or("0"))?
            {
                Ok(parent_id) => parent_id,
                Err(status) => return Ok(Err(status)),
            };
            if mailboxes.iter().any(|m| {
                m.id != id && m.parent_id == parent_id && m.name.eq_ignore_ascii_case(name)
            }) {
                return Ok(Err(status::EXISTS));
            }
            let patch = json!({ "name": name, "parentId": parent_id });
            jmap.set(
                crate::jmap::MAILBOX,
                crate::jmap::SetRequest::default().update(single(&id, patch)),
            )
            .await?
        }
        // Someone else's: not renamed from this user's phone.
        Target::Collection { id, .. } if id.contains('/') => {
            return Ok(Err(status::SPECIAL_FOLDER));
        }
        Target::Collection { data_type, id } => {
            jmap.set(
                data_type,
                crate::jmap::SetRequest::default().update(single(&id, json!({ "name": name }))),
            )
            .await?
        }
    };
    Ok(match failed(&result) {
        Some(status) => Err(status),
        None => Ok(server_id.to_owned()),
    })
}

async fn delete(
    req: &Request<'_>,
    jmap: &JmapClient,
    body: &Element,
) -> Result<Outcome, CommandError> {
    let Some(server_id) = body.child_text(fh::ServerId) else {
        return Ok(Err(status::MALFORMED));
    };
    let Some(stored) = stored_folder(req, server_id)? else {
        return Ok(Err(status::NOT_FOUND));
    };
    // Deleting an address book or calendar would delete everything in it;
    // not from a phone. Neither are someone else's mailboxes.
    let Target::Mailbox(id) = target(&stored) else {
        return Ok(Err(status::SPECIAL_FOLDER));
    };
    if id.contains('/') {
        return Ok(Err(status::SPECIAL_FOLDER));
    }
    let mailboxes = jmap.mailboxes().await?;
    let Some(mailbox) = mailboxes.iter().find(|m| m.id == id) else {
        return Ok(Err(status::NOT_FOUND));
    };
    if mailbox.role.is_some() {
        return Ok(Err(status::SPECIAL_FOLDER));
    }
    let trash = mailboxes
        .iter()
        .find(|m| m.role.as_deref() == Some("trash"));
    let result = match trash {
        Some(trash) if !is_inside(&mailboxes, mailbox, &trash.id) => {
            tracing::info!(
                name = mailbox.name,
                "device deleted a folder, moving it to Trash"
            );
            let patch = json!({ "parentId": trash.id });
            jmap.set(
                crate::jmap::MAILBOX,
                crate::jmap::SetRequest::default().update(single(&id, patch)),
            )
            .await?
        }
        // Already in Trash (or there is none): delete it for good.
        _ => {
            tracing::info!(name = mailbox.name, "device deleted a folder");
            jmap.set(
                crate::jmap::MAILBOX,
                crate::jmap::SetRequest::default()
                    .destroy(vec![id.clone()])
                    .with("onDestroyRemoveEmails", trash.is_some()),
            )
            .await?
        }
    };
    Ok(match failed(&result) {
        Some(status) => Err(status),
        None => Ok(server_id.to_owned()),
    })
}

fn stored_folder(req: &Request, server_id: &str) -> anyhow::Result<Option<String>> {
    match server_id.trim().parse() {
        Ok(id) => req.store.mailbox_for_folder(req.user, id),
        Err(_) => Ok(None),
    }
}

/// The JMAP mailbox for a ParentId (`None` for the root), or status 5.
fn parent_mailbox(
    req: &Request,
    parent: &str,
) -> Result<Result<Option<String>, &'static str>, CommandError> {
    if parent.trim() == "0" {
        return Ok(Ok(None));
    }
    Ok(match stored_folder(req, parent)?.map(|s| target(&s)) {
        // Folders are only made in the user's own account.
        Some(Target::Mailbox(id)) if !id.contains('/') => Ok(Some(id)),
        _ => Err(status::PARENT_NOT_FOUND),
    })
}

/// Whether `mailbox` is `ancestor` or below it.
fn is_inside(mailboxes: &[Mailbox], mailbox: &Mailbox, ancestor: &str) -> bool {
    let mut current = Some(mailbox);
    let mut steps = 0;
    while let Some(m) = current {
        if m.id == ancestor {
            return true;
        }
        steps += 1;
        if steps > mailboxes.len() {
            return false;
        }
        current = m
            .parent_id
            .as_deref()
            .and_then(|p| mailboxes.iter().find(|x| x.id == p));
    }
    false
}

fn single(id: &str, patch: Value) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(id.to_owned(), patch);
    map
}

/// The EAS status for a failed update or destroy, if it failed.
fn failed(result: &SetResult) -> Option<&'static str> {
    let error = result
        .not_updated
        .values()
        .chain(result.not_destroyed.values())
        .next()?;
    tracing::warn!(%error, "folder change rejected");
    Some(match error["type"].as_str() {
        Some("notFound") => status::NOT_FOUND,
        _ => status::SERVER_ERROR,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, parent: &str) -> Folder {
        Folder {
            server_id: id.to_owned(),
            parent_id: parent.to_owned(),
            name: id.to_owned(),
            folder_type: folder_type::USER_MAIL,
        }
    }

    #[test]
    fn finds_subtrees() {
        let folders = vec![
            folder("1", "0"),
            folder("2", "1"),
            folder("3", "2"),
            folder("4", "0"),
        ];
        let gone = subtree(&folders, "1");
        assert_eq!(
            gone,
            HashSet::from(["1".to_owned(), "2".to_owned(), "3".to_owned()])
        );
    }
}
