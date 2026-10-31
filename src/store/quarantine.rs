//! Sync loops: repeated SyncKeys per collection, and the items a device
//! kept failing on (see `eas::sync`).

use super::*;

/// How a device answered the responses for one collection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncRepeats {
    /// The SyncKey of the request the last response with items answered.
    pub sync_key: u32,
    /// How often in a row the device sent that key again.
    pub repeats: u32,
    /// When it last did (Unix seconds).
    pub last_at: i64,
    /// Whether responses carry one item at a time to find a broken one.
    pub probing: bool,
    /// The items of the last response while probing.
    pub sent: Vec<String>,
    /// The items of the response the device kept rejecting, which are
    /// probed one at a time.
    pub suspects: Vec<String>,
}

/// An item a device is not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedItem {
    pub collection_id: String,
    pub object_id: String,
    pub created_at: i64,
}

impl Store {
    pub fn sync_repeats(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<SyncRepeats>> {
        let row = self
            .conn
            .locked()
            .query_row(
                "SELECT sync_key, repeats, last_at, probing, sent, suspects FROM sync_repeats
                 WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
                params![user, device_id, collection_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(sync_key, repeats, last_at, probing, sent, suspects)| {
            Ok(SyncRepeats {
                sync_key,
                repeats,
                last_at,
                probing,
                sent: serde_json::from_str(&sent)?,
                suspects: serde_json::from_str(&suspects)?,
            })
        })
        .transpose()
    }

    pub fn save_sync_repeats(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        repeats: &SyncRepeats,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO sync_repeats (user, device_id, collection_id, sync_key, repeats,
                                       last_at, probing, sent, suspects)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (user, device_id, collection_id) DO UPDATE
             SET sync_key = excluded.sync_key, repeats = excluded.repeats,
                 last_at = excluded.last_at, probing = excluded.probing, sent = excluded.sent,
                 suspects = excluded.suspects",
            params![
                user,
                device_id,
                collection_id,
                repeats.sync_key,
                repeats.repeats,
                repeats.last_at,
                repeats.probing,
                serde_json::to_string(&repeats.sent)?,
                serde_json::to_string(&repeats.suspects)?,
            ],
        )?;
        Ok(())
    }

    pub fn delete_sync_repeats(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "DELETE FROM sync_repeats WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
            params![user, device_id, collection_id],
        )?;
        Ok(())
    }

    /// Stops sending `object_id` to the device.
    pub fn quarantine(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        object_id: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT OR IGNORE INTO quarantine (user, device_id, collection_id, object_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user, device_id, collection_id, object_id, unix_now()],
        )?;
        Ok(())
    }

    /// The quarantined items of one collection.
    pub fn quarantined(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        Ok(self
            .conn
            .locked()
            .prepare_cached(
                "SELECT object_id FROM quarantine
                 WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
            )?
            .query_map(params![user, device_id, collection_id], |row| row.get(0))?
            .collect::<Result<_, _>>()?)
    }

    /// Every quarantined item of the device.
    pub fn quarantine_list(
        &self,
        user: &str,
        device_id: &str,
    ) -> anyhow::Result<Vec<QuarantinedItem>> {
        Ok(self
            .conn
            .locked()
            .prepare(
                "SELECT collection_id, object_id, created_at FROM quarantine
                 WHERE user = ?1 AND device_id = ?2 ORDER BY collection_id, created_at",
            )?
            .query_map(params![user, device_id], |row| {
                Ok(QuarantinedItem {
                    collection_id: row.get(0)?,
                    object_id: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    /// Sends the device its quarantined items again (as new items at its
    /// next Sync); returns how many.
    pub fn clear_quarantine(&self, user: &str, device_id: &str) -> anyhow::Result<usize> {
        let items = self.quarantine_list(user, device_id)?;
        let collections: BTreeMap<&str, Vec<&str>> =
            items.iter().fold(BTreeMap::new(), |mut map, item| {
                map.entry(item.collection_id.as_str())
                    .or_default()
                    .push(item.object_id.as_str());
                map
            });
        let ids: Vec<&str> = collections.keys().copied().collect();
        self.update_collection_syncs(user, device_id, &ids, |collection_id, state| {
            let forget = |json: &mut String| -> anyhow::Result<()> {
                let mut snapshot: Snapshot = serde_json::from_str(json)?;
                for id in &collections[collection_id] {
                    snapshot.remove(*id);
                }
                *json = serde_json::to_string(&snapshot)?;
                Ok(())
            };
            forget(&mut state.snapshot)?;
            if let Some((_, previous)) = &mut state.previous {
                forget(previous)?;
            }
            Ok(())
        })?;
        let conn = self.conn.locked();
        conn.execute(
            "DELETE FROM sync_repeats WHERE user = ?1 AND device_id = ?2",
            params![user, device_id],
        )?;
        Ok(conn.execute(
            "DELETE FROM quarantine WHERE user = ?1 AND device_id = ?2",
            params![user, device_id],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_repeats_and_quarantine_per_collection() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.sync_repeats("rob", "d1", "5").unwrap(), None);
        let repeats = SyncRepeats {
            sync_key: 4,
            repeats: 2,
            last_at: 10,
            probing: true,
            sent: vec!["m1".to_owned()],
            suspects: vec!["m1".to_owned(), "m2".to_owned()],
        };
        store.save_sync_repeats("rob", "d1", "5", &repeats).unwrap();
        assert_eq!(store.sync_repeats("rob", "d1", "5").unwrap(), Some(repeats));

        store.quarantine("rob", "d1", "5", "m1").unwrap();
        store.quarantine("rob", "d1", "5", "m1").unwrap();
        store.quarantine("rob", "d1", "6", "c1").unwrap();
        assert_eq!(store.quarantined("rob", "d1", "5").unwrap(), ["m1"]);
        assert!(store.quarantined("rob", "d2", "5").unwrap().is_empty());
        assert_eq!(store.quarantine_list("rob", "d1").unwrap().len(), 2);
        assert_eq!(store.clear_quarantine("rob", "d1").unwrap(), 2);
        assert!(store.quarantined("rob", "d1", "5").unwrap().is_empty());
        assert_eq!(store.sync_repeats("rob", "d1", "5").unwrap(), None);
    }
}
