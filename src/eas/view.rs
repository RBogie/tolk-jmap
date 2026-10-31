//! The server side of a collection: its items with their versions and the
//! JMAP state that list reflects, for mail, contacts and calendars alike.
//!
//! A view is kept per device and collection and caught up from
//! `<type>/changes`, so an idle poll costs one small JMAP call instead of
//! listing the collection. It is only listed in full on the first sync,
//! when what it was built for changes (another folder or FilterType), or
//! when the server can no longer calculate changes.
//!
//! What differs per data type is a [`Source`]: how to list, how to refresh
//! changed items and what an item's version is. [`MailSource`] lives here;
//! contacts and calendars have theirs in their modules.

use std::{collections::HashSet, future::Future};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Request, email};
use crate::jmap::{Changes, JmapClient};

pub const READ: u64 = 1;
pub const FLAGGED: u64 = 2;
pub const ANSWERED: u64 = 4;
pub const FORWARDED: u64 = 8;

const CHANGES_PER_PAGE: usize = 500;
/// Beyond this many pages a full listing is cheaper than catching up.
const MAX_CHANGE_PAGES: usize = 10;

/// One item of a view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub version: u64,
    /// Sort key, newest first (mail: `receivedAt`); `None` keeps the order
    /// items were found in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct View {
    /// What the view was built for (see [`Source::key`]).
    key: String,
    state: String,
    items: Vec<Item>,
}

/// One kind of collection, as the view engine needs it.
pub trait Source {
    /// What the view is built for (collection and options); a stored view
    /// with another key is rebuilt.
    fn key(&self) -> String;
    /// The data type's current state string.
    fn state(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// One page of changes since `since`.
    fn changes(
        &self,
        since: &str,
        max: usize,
    ) -> impl Future<Output = anyhow::Result<Changes>> + Send;
    /// Every item that belongs in the view.
    fn list(&self) -> impl Future<Output = anyhow::Result<Vec<Item>>> + Send;
    /// The items among `ids` that (still) belong in the view.
    fn refresh(&self, ids: &[&str]) -> impl Future<Output = anyhow::Result<Vec<Item>>> + Send;
    /// Whether an item still belongs without having changed (a moving date
    /// window).
    fn keeps(&self, _item: &Item) -> bool {
        true
    }
}

/// The collection's current items with their versions, newest first.
pub async fn current_items(
    req: &Request<'_>,
    collection_id: &str,
    source: &(impl Source + Sync),
) -> anyhow::Result<Vec<(String, u64)>> {
    let key = source.key();
    let stored = req
        .store
        .collection_view(req.user, req.device_id, collection_id)?
        .and_then(|json| serde_json::from_str::<View>(&json).ok())
        .filter(|v| v.key == key);
    let caught_up = match stored {
        Some(view) => catch_up(source, view).await?,
        None => None,
    };
    let mut view = match caught_up {
        Some(view) => view,
        None => {
            // Taken before listing, so changes made meanwhile are replayed
            // next time.
            let state = source.state().await?;
            let items = source.list().await?;
            tracing::debug!(key, items = items.len(), "full listing");
            View { key, state, items }
        }
    };
    view.items.retain(|item| source.keeps(item));
    if req.persist_views {
        req.store.save_collection_view(
            req.user,
            req.device_id,
            collection_id,
            &serde_json::to_string(&view)?,
        )?;
    }
    Ok(view.items.into_iter().map(|i| (i.id, i.version)).collect())
}

/// Drops items from a collection's stored view.
pub fn forget(req: &Request, collection_id: &str, ids: &[&str]) -> anyhow::Result<()> {
    let Some(json) = req
        .store
        .collection_view(req.user, req.device_id, collection_id)?
    else {
        return Ok(());
    };
    let Ok(mut view) = serde_json::from_str::<View>(&json) else {
        return Ok(());
    };
    view.items.retain(|item| !ids.contains(&item.id.as_str()));
    req.store.save_collection_view(
        req.user,
        req.device_id,
        collection_id,
        &serde_json::to_string(&view)?,
    )
}

/// Applies the changes since the view's state; `None` means the server
/// cannot tell and the collection has to be listed again.
async fn catch_up(source: &impl Source, mut view: View) -> anyhow::Result<Option<View>> {
    let mut touched = HashSet::new();
    let mut destroyed = HashSet::new();
    let mut state = view.state.clone();
    for page in 1.. {
        let Changes::Changes {
            new_state,
            has_more,
            created,
            updated,
            destroyed: gone,
        } = source.changes(&state, CHANGES_PER_PAGE).await?
        else {
            return Ok(None);
        };
        touched.extend(created);
        touched.extend(updated);
        for id in gone {
            touched.remove(&id);
            destroyed.insert(id);
        }
        state = new_state;
        if !has_more {
            break;
        }
        if page == MAX_CHANGE_PAGES {
            return Ok(None);
        }
    }
    tracing::debug!(
        key = view.key,
        since = view.state,
        new_state = state,
        touched = touched.len(),
        destroyed = destroyed.len(),
        "caught up"
    );
    view.state = state;
    if touched.is_empty() && destroyed.is_empty() {
        return Ok(Some(view));
    }
    let ids: Vec<&str> = touched.iter().map(String::as_str).collect();
    let refreshed = source.refresh(&ids).await?;
    merge(&mut view.items, &touched, &destroyed, refreshed);
    Ok(Some(view))
}

/// Replaces every touched or destroyed item with its refreshed version (if
/// it still belongs), newest first.
fn merge(
    items: &mut Vec<Item>,
    touched: &HashSet<String>,
    destroyed: &HashSet<String>,
    refreshed: Vec<Item>,
) {
    items.retain(|i| !touched.contains(&i.id) && !destroyed.contains(&i.id));
    items.extend(refreshed);
    if items.iter().any(|i| i.order.is_some()) {
        items.sort_by(|a, b| b.order.cmp(&a.order).then_with(|| b.id.cmp(&a.id)));
    }
}

/// A mail folder's emails in the sync window, versioned by their flags.
pub struct MailSource<'a> {
    pub jmap: &'a JmapClient,
    pub mailbox_id: &'a str,
    /// The sync window in days (see [`window_days`]), `None` for all.
    pub days: Option<u32>,
    /// The window start, if any.
    pub window: Option<String>,
}

impl<'a> MailSource<'a> {
    pub fn new(jmap: &'a JmapClient, mailbox_id: &'a str, days: Option<u32>) -> Self {
        Self {
            jmap,
            mailbox_id,
            days,
            window: window_start(days),
        }
    }

    fn item(&self, meta: crate::jmap::EmailMeta) -> Option<Item> {
        let in_folder = meta.mailbox_ids.get(self.mailbox_id) == Some(&true);
        let in_window = self
            .window
            .as_deref()
            .is_none_or(|start| meta.received_at.as_str() >= start);
        (in_folder && in_window).then(|| Item {
            version: flags(&meta.keywords),
            id: meta.id,
            order: Some(meta.received_at),
        })
    }
}

impl Source for MailSource<'_> {
    fn key(&self) -> String {
        format!("mail:{}:{}d", self.mailbox_id, self.days.unwrap_or(0))
    }

    async fn state(&self) -> anyhow::Result<String> {
        self.jmap.state(crate::jmap::EMAIL).await
    }

    async fn changes(&self, since: &str, max: usize) -> anyhow::Result<Changes> {
        self.jmap.changes(crate::jmap::EMAIL, since, max).await
    }

    async fn list(&self) -> anyhow::Result<Vec<Item>> {
        let mut filter = json!({ "inMailbox": self.mailbox_id });
        if let Some(start) = &self.window {
            filter["after"] = start.as_str().into();
        }
        let ids = self.jmap.query_email_ids(filter).await?;
        let mut items = self
            .refresh(&ids.iter().map(String::as_str).collect::<Vec<_>>())
            .await?;
        items.sort_by(|a, b| b.order.cmp(&a.order).then_with(|| b.id.cmp(&a.id)));
        Ok(items)
    }

    async fn refresh(&self, ids: &[&str]) -> anyhow::Result<Vec<Item>> {
        let metas = self.jmap.email_meta(ids).await?;
        Ok(metas.into_iter().filter_map(|m| self.item(m)).collect())
    }

    // The window moves with time, so old emails drop out without a change.
    fn keeps(&self, item: &Item) -> bool {
        match (&self.window, &item.order) {
            (Some(start), Some(at)) => at >= start,
            _ => true,
        }
    }
}

/// An email's version: its read, flagged, answered and forwarded state.
pub fn flags(keywords: &std::collections::HashMap<String, bool>) -> u64 {
    let has = |k: &str| keywords.get(k) == Some(&true);
    [
        (email::SEEN, READ),
        (email::FLAGGED, FLAGGED),
        (email::ANSWERED, ANSWERED),
        (email::FORWARDED, FORWARDED),
    ]
    .into_iter()
    .filter(|(keyword, _)| has(keyword))
    .fold(0, |flags, (_, bit)| flags | bit)
}

/// The days an EAS FilterType goes back (1 day to 6 months); `None` is
/// everything.
pub fn filter_days(filter_type: u8) -> Option<u32> {
    Some(match filter_type {
        1 => 1,
        2 => 3,
        3 => 7,
        4 => 14,
        5 => 30,
        6 => 90,
        7 => 180,
        _ => return None,
    })
}

/// The sync window in days: the device's FilterType, but no further back
/// than `max_days` (`SYNC_MAX_AGE_DAYS`); `None` is everything.
pub fn window_days(filter_type: u8, max_days: Option<u32>) -> Option<u32> {
    match (filter_days(filter_type), max_days) {
        (Some(days), Some(max)) => Some(days.min(max)),
        (days, max) => days.or(max),
    }
}

/// Start of a sync window of `days`, as a JMAP UTCDate.
pub fn window_start(days: Option<u32>) -> Option<String> {
    let days = i64::from(days?);
    Some(
        (chrono::Utc::now() - chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn item(id: &str, version: u64, at: &str) -> Item {
        Item {
            id: id.to_owned(),
            version,
            order: Some(at.to_owned()),
        }
    }

    #[test]
    fn merges_changes_into_view() {
        let mut items = vec![
            item("c", 0, "2026-10-30T10:00:00Z"),
            item("b", 0, "2026-10-29T10:00:00Z"),
            item("a", 0, "2026-10-28T10:00:00Z"),
        ];
        let touched: HashSet<String> = ["a", "b", "new"].map(str::to_owned).into();
        let destroyed: HashSet<String> = ["c".to_owned()].into();
        // b moved away (not refreshed), a was read, new arrived.
        let refreshed = vec![
            item("a", READ, "2026-10-28T10:00:00Z"),
            item("new", 0, "2026-10-30T12:00:00Z"),
        ];
        merge(&mut items, &touched, &destroyed, refreshed);
        assert_eq!(
            items,
            vec![
                item("new", 0, "2026-10-30T12:00:00Z"),
                item("a", READ, "2026-10-28T10:00:00Z")
            ]
        );
    }

    #[test]
    fn reads_flags_from_keywords() {
        let keywords: HashMap<String, bool> =
            [("$seen".to_owned(), true), ("$flagged".to_owned(), true)].into();
        assert_eq!(flags(&keywords), READ | FLAGGED);
        assert_eq!(flags(&HashMap::new()), 0);
    }

    #[test]
    fn windows_follow_the_filter_type() {
        assert!(window_start(filter_days(0)).is_none());
        let week = window_start(filter_days(3)).unwrap();
        let expected = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        assert!(week.starts_with(&expected), "{week}");
    }

    #[test]
    fn the_age_cap_limits_every_window() {
        assert_eq!(window_days(0, None), None);
        assert_eq!(window_days(0, Some(60)), Some(60));
        assert_eq!(window_days(6, Some(60)), Some(60));
        assert_eq!(window_days(3, Some(60)), Some(7));
        assert_eq!(window_days(5, None), Some(30));
    }
}
