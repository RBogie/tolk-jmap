//! FolderSync (MS-ASCMD 2.2.1.5): the folder hierarchy, from JMAP mailboxes.
//!
//! Instead of `Mailbox/changes`, every request fetches all mailboxes and
//! diffs them against the snapshot the device holds for its SyncKey. Mailbox
//! counts are small, and this stays correct whatever the JMAP server reports.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{CommandError, CommandResult, Request, calendar, contacts, sync};
use crate::{
    jmap::{self, JmapClient, Mailbox},
    wbxml::{Element, cp::folder_hierarchy as fh},
};

mod status {
    pub const OK: &str = "1";
    pub const INVALID_SYNC_KEY: &str = "9";
}

/// EAS folder types (MS-ASCMD 2.2.3.186.3).
pub(super) mod folder_type {
    pub const INBOX: u8 = 2;
    pub const DRAFTS: u8 = 3;
    pub const DELETED_ITEMS: u8 = 4;
    pub const SENT_ITEMS: u8 = 5;
    pub const DEFAULT_CALENDAR: u8 = 8;
    pub const DEFAULT_CONTACTS: u8 = 9;
    pub const USER_MAIL: u8 = 12;
    pub const USER_CALENDAR: u8 = 13;
    pub const USER_CONTACTS: u8 = 14;
}

const ROOT: &str = "0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub server_id: String,
    pub parent_id: String,
    pub name: String,
    pub folder_type: u8,
}

#[derive(Debug, PartialEq, Eq)]
enum Change<'a> {
    Add(&'a Folder),
    Update(&'a Folder),
    Delete(&'a str),
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(fh::FolderSync)?;
    let key = body
        .child_text(fh::SyncKey)
        .ok_or_else(|| CommandError::bad("FolderSync without SyncKey"))?;
    let Ok(key) = key.parse::<u32>() else {
        return Ok(invalid_key());
    };
    if key == 0 {
        // The device starts over (new account or reset): its old per-folder
        // state can only cause stale replies.
        req.store.reset_collections(req.user, req.device_id)?;
    }
    let current = current_folders(req, jmap).await?;
    sync(req, key, &current)
}

/// Whether the account's folders differ from what the device last got from
/// FolderSync (it should FolderSync again); false before its first one.
pub(super) async fn hierarchy_changed(
    req: &Request<'_>,
    jmap: &JmapClient,
) -> Result<bool, CommandError> {
    let Some(state) = req.store.folder_sync_state(req.user, req.device_id)? else {
        return Ok(false);
    };
    let known = parse_snapshot(&state.snapshot)?;
    let current = current_folders(req, jmap).await?;
    Ok(!diff(&known, &current).is_empty())
}

/// Mailboxes, address books and calendars as EAS folders.
pub(super) async fn current_folders(
    req: &Request<'_>,
    jmap: &JmapClient,
) -> Result<Vec<Folder>, CommandError> {
    let mailboxes = jmap.mailboxes().await?;
    let mut current = folders(req, None, &mailboxes)?;
    if jmap.has_contacts() {
        let books = address_books(jmap).await?;
        current.extend(collection_folders(
            req,
            contacts::PREFIX,
            None,
            &books,
            CONTACT_TYPES,
            "Contacts",
        )?);
    }
    if jmap.has_calendars() {
        let calendars = calendars(jmap).await?;
        current.extend(collection_folders(
            req,
            calendar::PREFIX,
            None,
            &calendars,
            CALENDAR_TYPES,
            "Calendar",
        )?);
    }
    // Address books and calendars other users share with this one. A
    // shared account may advertise a capability without serving it.
    fn offered<T>(result: anyhow::Result<Vec<T>>, account: &str) -> anyhow::Result<Vec<T>> {
        match result {
            Err(e) if jmap::is_not_offered(&e) => {
                tracing::debug!(account, error = format!("{e:#}"), "nothing shared");
                Ok(Vec::new())
            }
            result => result,
        }
    }
    for (account, owner) in jmap.shared_mail_accounts() {
        let mailboxes = offered(jmap.in_account(account).mailboxes().await, account)?;
        current.extend(folders(req, Some((account, owner)), &mailboxes)?);
    }
    for (account, owner) in jmap.shared_contact_accounts() {
        let books = offered(address_books(&jmap.in_account(account)).await, account)?;
        current.extend(collection_folders(
            req,
            contacts::PREFIX,
            Some((account, owner)),
            &books,
            CONTACT_TYPES,
            "Contacts",
        )?);
    }
    for (account, owner) in jmap.shared_calendar_accounts() {
        let calendars = offered(calendars(&jmap.in_account(account)).await, account)?;
        current.extend(collection_folders(
            req,
            calendar::PREFIX,
            Some((account, owner)),
            &calendars,
            CALENDAR_TYPES,
            "Calendar",
        )?);
    }
    Ok(current)
}

/// The account's address books as (id, name, is default).
async fn address_books(jmap: &JmapClient) -> anyhow::Result<Vec<(String, String, bool)>> {
    Ok(jmap
        .address_books()
        .await?
        .into_iter()
        .map(|b| (b.id, b.name, b.is_default))
        .collect())
}

/// The account's calendars as (id, name, is default).
async fn calendars(jmap: &JmapClient) -> anyhow::Result<Vec<(String, String, bool)>> {
    Ok(jmap
        .calendars()
        .await?
        .into_iter()
        .map(|c| (c.id, c.name, c.is_default))
        .collect())
}

/// Answers a FolderSync with `key` given the account's `current` folders.
fn sync(req: &Request, key: u32, current: &[Folder]) -> CommandResult {
    let state = req.store.folder_sync_state(req.user, req.device_id)?;
    let base: Vec<Folder> = match &state {
        _ if key == 0 => Vec::new(),
        Some(s) if s.sync_key == key => parse_snapshot(&s.snapshot)?,
        Some(s) => match &s.previous {
            Some((prev_key, snapshot)) if *prev_key == key => parse_snapshot(snapshot)?,
            _ => return Ok(invalid_key()),
        },
        None => return Ok(invalid_key()),
    };

    let changes = diff(&base, current);
    if changes.is_empty() && key != 0 {
        return Ok(response(key, &changes));
    }
    let new_key = state.as_ref().map_or(0, |s| s.sync_key).max(key) + 1;
    let base_json = serde_json::to_string(&base).map_err(anyhow::Error::from)?;
    req.store.save_folder_sync(
        req.user,
        req.device_id,
        (key != 0).then_some((key, base_json.as_str())),
        new_key,
        &serde_json::to_string(current).map_err(anyhow::Error::from)?,
    )?;
    Ok(response(new_key, &changes))
}

pub(super) fn parse_snapshot(json: &str) -> Result<Vec<Folder>, CommandError> {
    Ok(serde_json::from_str(json).map_err(anyhow::Error::from)?)
}

/// Maps mailboxes to EAS folders, parents before children. Those of a
/// shared account (id, owner name) are plain mail folders, never the
/// device's Inbox or Sent, and the top ones carry the owner's name.
fn folders(
    req: &Request,
    shared: Option<(&str, &str)>,
    mailboxes: &[Mailbox],
) -> Result<Vec<Folder>, CommandError> {
    let account = shared.map(|(id, _)| id);
    // In mailbox order: new folders are numbered in this order.
    let keys: Vec<String> = mailboxes
        .iter()
        .map(|m| sync::stored_id("", account, &m.id))
        .collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let stored = req.store.folder_server_ids(req.user, &key_refs)?;
    let ids: HashMap<&str, u32> = mailboxes
        .iter()
        .zip(&keys)
        .map(|(m, key)| (m.id.as_str(), stored[key.as_str()]))
        .collect();
    let by_id: HashMap<&str, &Mailbox> = mailboxes.iter().map(|m| (m.id.as_str(), m)).collect();

    let mut ordered: Vec<(usize, &Mailbox)> =
        mailboxes.iter().map(|m| (depth(m, &by_id), m)).collect();
    ordered
        .sort_by(|(da, a), (db, b)| (da, a.sort_order, &a.name).cmp(&(db, b.sort_order, &b.name)));

    Ok(ordered
        .into_iter()
        .map(|(_, m)| {
            let parent = m.parent_id.as_deref().and_then(|p| ids.get(p));
            Folder {
                server_id: ids[m.id.as_str()].to_string(),
                parent_id: parent.map_or_else(|| ROOT.to_owned(), u32::to_string),
                name: match shared {
                    Some((_, owner)) if parent.is_none() && !owner.is_empty() => {
                        format!("{} ({owner})", m.name)
                    }
                    _ => m.name.clone(),
                },
                folder_type: match shared {
                    Some(_) => folder_type::USER_MAIL,
                    None => folder_type(m.role.as_deref()),
                },
            }
        })
        .collect())
}

fn depth(mailbox: &Mailbox, by_id: &HashMap<&str, &Mailbox>) -> usize {
    let mut depth = 0;
    let mut parent = mailbox.parent_id.as_deref();
    // The bound guards against a parent cycle in broken server data.
    while let Some(p) = parent
        .and_then(|p| by_id.get(p))
        .filter(|_| depth < by_id.len())
    {
        depth += 1;
        parent = p.parent_id.as_deref();
    }
    depth
}

/// EAS folder types for (the default, any other) address book or calendar.
const CONTACT_TYPES: (u8, u8) = (folder_type::DEFAULT_CONTACTS, folder_type::USER_CONTACTS);
const CALENDAR_TYPES: (u8, u8) = (folder_type::DEFAULT_CALENDAR, folder_type::USER_CALENDAR);

/// Address books or calendars (id, name, is default) as folders: the
/// default one (or the first) as the device's main folder of that kind.
/// Those of a shared account (id, owner name) are never the main folder
/// and carry the owner's name.
fn collection_folders(
    req: &Request,
    prefix: &str,
    shared: Option<(&str, &str)>,
    collections: &[(String, String, bool)],
    (main_type, other_type): (u8, u8),
    fallback_name: &str,
) -> Result<Vec<Folder>, CommandError> {
    let account = shared.map(|(id, _)| id);
    let keys: Vec<String> = collections
        .iter()
        .map(|(id, _, _)| sync::stored_id(prefix, account, id))
        .collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ids = req.store.folder_server_ids(req.user, &key_refs)?;
    let main = match shared {
        Some(_) => None,
        None => Some(
            collections
                .iter()
                .position(|(_, _, default)| *default)
                .unwrap_or(0),
        ),
    };
    Ok(collections
        .iter()
        .zip(&keys)
        .enumerate()
        .map(|(i, ((_, name, _), key))| {
            let name = if name.is_empty() { fallback_name } else { name };
            Folder {
                server_id: ids[key].to_string(),
                parent_id: ROOT.to_owned(),
                name: match shared {
                    Some((_, owner)) if !owner.is_empty() => format!("{name} ({owner})"),
                    _ => name.to_owned(),
                },
                folder_type: if Some(i) == main {
                    main_type
                } else {
                    other_type
                },
            }
        })
        .collect())
}

fn folder_type(role: Option<&str>) -> u8 {
    match role {
        Some("inbox") => folder_type::INBOX,
        Some("drafts") => folder_type::DRAFTS,
        Some("trash") => folder_type::DELETED_ITEMS,
        Some("sent") => folder_type::SENT_ITEMS,
        _ => folder_type::USER_MAIL,
    }
}

fn diff<'a>(base: &'a [Folder], current: &'a [Folder]) -> Vec<Change<'a>> {
    let before: HashMap<&str, &Folder> = base.iter().map(|f| (f.server_id.as_str(), f)).collect();
    let now: HashMap<&str, &Folder> = current.iter().map(|f| (f.server_id.as_str(), f)).collect();
    let mut changes: Vec<Change> = current
        .iter()
        .filter_map(|f| match before.get(f.server_id.as_str()) {
            None => Some(Change::Add(f)),
            Some(old) if *old != f => Some(Change::Update(f)),
            Some(_) => None,
        })
        .collect();
    changes.extend(
        base.iter()
            .filter(|f| !now.contains_key(f.server_id.as_str()))
            .map(|f| Change::Delete(&f.server_id)),
    );
    changes
}

fn response(key: u32, changes: &[Change]) -> Element {
    let mut list =
        Element::new(fh::Changes).child(Element::with_text(fh::Count, changes.len().to_string()));
    for change in changes {
        list.push(match change {
            Change::Add(f) => folder_element(fh::Add, f),
            Change::Update(f) => folder_element(fh::Update, f),
            Change::Delete(id) => {
                Element::new(fh::Delete).child(Element::with_text(fh::ServerId, *id))
            }
        });
    }
    Element::new(fh::FolderSync)
        .child(Element::with_text(fh::Status, status::OK))
        .child(Element::with_text(fh::SyncKey, key.to_string()))
        .child(list)
}

fn folder_element(tag: crate::wbxml::Tag, folder: &Folder) -> Element {
    Element::new(tag)
        .child(Element::with_text(fh::ServerId, &folder.server_id))
        .child(Element::with_text(fh::ParentId, &folder.parent_id))
        .child(Element::with_text(fh::DisplayName, &folder.name))
        .child(Element::with_text(fh::Type, folder.folder_type.to_string()))
}

fn invalid_key() -> Element {
    Element::new(fh::FolderSync).child(Element::with_text(fh::Status, status::INVALID_SYNC_KEY))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn mailbox(id: &str, name: &str, parent: Option<&str>, role: Option<&str>) -> Mailbox {
        Mailbox {
            id: id.to_owned(),
            name: name.to_owned(),
            parent_id: parent.map(str::to_owned),
            role: role.map(str::to_owned),
            sort_order: 0,
        }
    }

    fn changes(response: &Element) -> Vec<(String, Option<&str>)> {
        response
            .find(fh::Changes)
            .unwrap()
            .elements()
            .filter(|e| e.tag != fh::Count)
            .map(|e| {
                (
                    e.tag.to_string(),
                    e.child_text(fh::DisplayName).or(e.child_text(fh::ServerId)),
                )
            })
            .collect()
    }

    #[test]
    fn maps_mailboxes_parents_first() {
        let store = Store::open_in_memory().unwrap();
        let req = Request::for_tests(&store, None);
        let mailboxes = [
            mailbox("c", "Child", Some("p"), None),
            mailbox("p", "Projects", None, None),
            mailbox("i", "Inbox", None, Some("inbox")),
            mailbox("t", "Trash", None, Some("trash")),
        ];
        let folders = folders(&req, None, &mailboxes).unwrap();
        let names: Vec<_> = folders
            .iter()
            .map(|f| (f.name.as_str(), f.folder_type))
            .collect();
        assert_eq!(
            names,
            vec![("Inbox", 2), ("Projects", 12), ("Trash", 4), ("Child", 12)]
        );
        let projects = folders.iter().find(|f| f.name == "Projects").unwrap();
        let child = folders.iter().find(|f| f.name == "Child").unwrap();
        assert_eq!(child.parent_id, projects.server_id);
        assert_eq!(projects.parent_id, ROOT);
    }

    #[test]
    fn sync_key_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let req = Request::for_tests(&store, None);
        let v1 = folders(
            &req,
            None,
            &[
                mailbox("i", "Inbox", None, Some("inbox")),
                mailbox("a", "A", None, None),
            ],
        )
        .unwrap();

        // Unknown key before any sync.
        assert_eq!(
            sync(&req, 5, &v1).unwrap().child_text(fh::Status),
            Some("9")
        );

        // Initial sync sends everything with key 1.
        let first = sync(&req, 0, &v1).unwrap();
        assert_eq!(first.child_text(fh::SyncKey), Some("1"));
        assert_eq!(changes(&first).len(), 2);

        // No changes: same key, empty change list.
        let same = sync(&req, 1, &v1).unwrap();
        assert_eq!(same.child_text(fh::SyncKey), Some("1"));
        assert!(changes(&same).is_empty());

        // Rename A, delete Inbox, add B.
        let v2 = folders(
            &req,
            None,
            &[
                mailbox("a", "A2", None, None),
                mailbox("b", "B", None, None),
            ],
        )
        .unwrap();
        let second = sync(&req, 1, &v2).unwrap();
        assert_eq!(second.child_text(fh::SyncKey), Some("2"));
        let inbox_id = v1
            .iter()
            .find(|f| f.name == "Inbox")
            .unwrap()
            .server_id
            .clone();
        assert_eq!(
            changes(&second),
            vec![
                ("FolderHierarchy:Update".to_owned(), Some("A2")),
                ("FolderHierarchy:Add".to_owned(), Some("B")),
                ("FolderHierarchy:Delete".to_owned(), Some(inbox_id.as_str())),
            ]
        );

        // The response got lost: retrying with key 1 replays the same changes.
        let retry = sync(&req, 1, &v2).unwrap();
        assert_eq!(retry.child_text(fh::SyncKey), Some("3"));
        assert_eq!(changes(&retry), changes(&second));

        // Key 2 is now outdated, key 3 is current.
        assert_eq!(
            sync(&req, 2, &v2).unwrap().child_text(fh::Status),
            Some("9")
        );
        assert!(changes(&sync(&req, 3, &v2).unwrap()).is_empty());
    }
}
