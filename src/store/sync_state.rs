//! What each device was sent: FolderSync and Sync state with their
//! snapshots, cached server views, Ping parameters and the last Sync
//! request.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSyncState {
    pub sync_key: u32,
    pub snapshot: String,
    pub previous: Option<(u32, String)>,
}

/// A collection as `devices show` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSummary {
    pub collection_id: String,
    pub sync_key: u32,
    /// Items the device has.
    pub items: usize,
    pub options: Option<String>,
    /// When its state last changed (Unix seconds).
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSyncState {
    pub sync_key: u32,
    pub snapshot: String,
    pub previous: Option<(u32, String)>,
    pub options: Option<String>,
}

impl Store {
    /// Drops the device's per-collection state (a FolderSync from key 0
    /// means the device starts over).
    pub fn reset_collections(&self, user: &str, device_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.locked();
        for table in [
            "collection_sync",
            "collection_view",
            "ping_params",
            "sync_requests",
            "sync_repeats",
        ] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE user = ?1 AND device_id = ?2"),
                params![user, device_id],
            )?;
        }
        Ok(())
    }

    /// Makes one collection start over: the device's next Sync of it gets
    /// "invalid SyncKey" and syncs it again from key 0.
    pub fn resync_collection(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.locked();
        let mut found = false;
        for table in ["collection_sync", "collection_view", "sync_repeats"] {
            found |= conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3"
                ),
                params![user, device_id, collection_id],
            )? > 0;
        }
        Ok(found)
    }

    /// Makes the device start over without forgetting it: FolderSync gets
    /// "invalid SyncKey" (status 9) and the device syncs everything again.
    pub fn resync_device(&self, user: &str, device_id: &str) -> anyhow::Result<()> {
        self.reset_collections(user, device_id)?;
        self.conn.locked().execute(
            "DELETE FROM folder_sync WHERE user = ?1 AND device_id = ?2",
            params![user, device_id],
        )?;
        Ok(())
    }

    /// The device's collections with their state, by collection id.
    pub fn collection_summaries(
        &self,
        user: &str,
        device_id: &str,
    ) -> anyhow::Result<Vec<CollectionSummary>> {
        let conn = self.conn.locked();
        let mut statement = conn.prepare(
            "SELECT collection_id, sync_key, snapshot, options, updated_at FROM collection_sync
             WHERE user = ?1 AND device_id = ?2",
        )?;
        let mut rows: Vec<CollectionSummary> = statement
            .query_map(params![user, device_id], |row| {
                let snapshot: String = row.get(2)?;
                Ok(CollectionSummary {
                    collection_id: row.get(0)?,
                    sync_key: row.get(1)?,
                    items: serde_json::from_str::<Snapshot>(&snapshot)
                        .map(|s| s.len())
                        .unwrap_or_default(),
                    options: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        rows.sort_by_key(|r| r.collection_id.parse::<u32>().unwrap_or(u32::MAX));
        Ok(rows)
    }

    pub fn sync_request(&self, user: &str, device_id: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT request FROM sync_requests WHERE user = ?1 AND device_id = ?2",
                params![user, device_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_sync_request(
        &self,
        user: &str,
        device_id: &str,
        request: &[u8],
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO sync_requests (user, device_id, request) VALUES (?1, ?2, ?3)
             ON CONFLICT (user, device_id) DO UPDATE SET request = excluded.request",
            params![user, device_id, request],
        )?;
        Ok(())
    }

    pub fn ping_params(&self, user: &str, device_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT params FROM ping_params WHERE user = ?1 AND device_id = ?2",
                params![user, device_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_ping_params(
        &self,
        user: &str,
        device_id: &str,
        params: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO ping_params (user, device_id, params) VALUES (?1, ?2, ?3)
             ON CONFLICT (user, device_id) DO UPDATE SET params = excluded.params",
            params![user, device_id, params],
        )?;
        Ok(())
    }

    pub fn collection_view(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT view FROM collection_view
                 WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
                params![user, device_id, collection_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_collection_view(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        view: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO collection_view (user, device_id, collection_id, view)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user, device_id, collection_id) DO UPDATE SET view = excluded.view",
            params![user, device_id, collection_id, view],
        )?;
        Ok(())
    }

    pub fn collection_sync_state(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<CollectionSyncState>> {
        read_collection_sync(&self.conn.locked(), user, device_id, collection_id)
    }

    pub fn save_collection_sync(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        state: &CollectionSyncState,
    ) -> anyhow::Result<()> {
        write_collection_sync(&self.conn.locked(), user, device_id, collection_id, state)
    }

    /// Changes the stored state of several collections in one transaction;
    /// collections without state are skipped.
    pub fn update_collection_syncs(
        &self,
        user: &str,
        device_id: &str,
        collection_ids: &[&str],
        mut update: impl FnMut(&str, &mut CollectionSyncState) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.locked();
        let tx = conn.transaction()?;
        for &collection_id in collection_ids {
            let Some(mut state) = read_collection_sync(&tx, user, device_id, collection_id)? else {
                continue;
            };
            update(collection_id, &mut state)?;
            write_collection_sync(&tx, user, device_id, collection_id, &state)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn folder_sync_state(
        &self,
        user: &str,
        device_id: &str,
    ) -> anyhow::Result<Option<FolderSyncState>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT sync_key, snapshot, prev_sync_key, prev_snapshot FROM folder_sync
                 WHERE user = ?1 AND device_id = ?2",
                params![user, device_id],
                |row| {
                    let prev_key: Option<u32> = row.get(2)?;
                    let prev_snapshot: Option<String> = row.get(3)?;
                    Ok(FolderSyncState {
                        sync_key: row.get(0)?,
                        snapshot: row.get(1)?,
                        previous: prev_key.zip(prev_snapshot),
                    })
                },
            )
            .optional()?)
    }

    pub fn save_folder_sync(
        &self,
        user: &str,
        device_id: &str,
        previous: Option<(u32, &str)>,
        sync_key: u32,
        snapshot: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO folder_sync (user, device_id, sync_key, snapshot, prev_sync_key, prev_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (user, device_id) DO UPDATE
             SET sync_key = excluded.sync_key, snapshot = excluded.snapshot,
                 prev_sync_key = excluded.prev_sync_key, prev_snapshot = excluded.prev_snapshot",
            params![
                user,
                device_id,
                sync_key,
                snapshot,
                previous.map(|p| p.0),
                previous.map(|p| p.1)
            ],
        )?;
        Ok(())
    }
}

fn read_collection_sync(
    conn: &Connection,
    user: &str,
    device_id: &str,
    collection_id: &str,
) -> anyhow::Result<Option<CollectionSyncState>> {
    Ok(conn
        .query_row(
            "SELECT sync_key, snapshot, prev_sync_key, prev_snapshot, options
             FROM collection_sync
             WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
            params![user, device_id, collection_id],
            |row| {
                let prev_key: Option<u32> = row.get(2)?;
                let prev_snapshot: Option<String> = row.get(3)?;
                Ok(CollectionSyncState {
                    sync_key: row.get(0)?,
                    snapshot: row.get(1)?,
                    previous: prev_key.zip(prev_snapshot),
                    options: row.get(4)?,
                })
            },
        )
        .optional()?)
}

fn write_collection_sync(
    conn: &Connection,
    user: &str,
    device_id: &str,
    collection_id: &str,
    state: &CollectionSyncState,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO collection_sync (user, device_id, collection_id, sync_key, snapshot,
                                      prev_sync_key, prev_snapshot, options, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (user, device_id, collection_id) DO UPDATE
         SET sync_key = excluded.sync_key, snapshot = excluded.snapshot,
             prev_sync_key = excluded.prev_sync_key, prev_snapshot = excluded.prev_snapshot,
             options = excluded.options, updated_at = excluded.updated_at",
        params![
            user,
            device_id,
            collection_id,
            state.sync_key,
            state.snapshot,
            state.previous.as_ref().map(|p| p.0),
            state.previous.as_ref().map(|p| p.1.as_str()),
            state.options,
            unix_now(),
        ],
    )?;
    Ok(())
}
