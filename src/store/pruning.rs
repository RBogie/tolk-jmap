//! Forgetting idle devices, old ClientIds and item ids no device holds.

use super::*;

/// How long pruned state is kept (see [`Store::prune`]).
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// Devices not seen for this long are forgotten with all their state.
    pub idle_device: Duration,
    /// SendMail and Sync Add ClientIds are remembered this long.
    pub client_ids: Duration,
    /// Item ids no device holds are kept at least this long.
    pub item_grace: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        const DAY: u64 = 86_400;
        Self {
            idle_device: Duration::from_secs(180 * DAY),
            client_ids: Duration::from_secs(30 * DAY),
            item_grace: Duration::from_secs(DAY),
        }
    }
}

/// What [`Store::prune`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub devices: usize,
    pub client_ids: usize,
    pub item_ids: usize,
}

impl Store {
    /// Forgets idle devices, old ClientIds and item ids no device holds.
    ///
    /// Item ids are shared by a user's devices, so one is only dropped when
    /// no device's current or previous snapshot of that folder has the item
    /// and it is older than the grace period (a Sync assigns ids before it
    /// saves the snapshot that holds them).
    pub fn prune(&self, retention: &Retention) -> anyhow::Result<Pruned> {
        let now = unix_now();
        let before = |age: Duration| now - age.as_secs() as i64;
        let mut conn = self.conn.locked();
        let tx = conn.transaction()?;
        let mut pruned = Pruned::default();

        let idle: Vec<(String, String)> = tx
            .prepare("SELECT user, device_id FROM devices WHERE last_seen < ?1")?
            .query_map([before(retention.idle_device)], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        for (user, device_id) in &idle {
            for table in DEVICE_TABLES {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE user = ?1 AND device_id = ?2"),
                    params![user, device_id],
                )?;
            }
            tracing::info!(user, device_id, "forgot idle device");
        }
        pruned.devices = idle.len();

        let cutoff = before(retention.client_ids);
        pruned.client_ids = tx
            .execute("DELETE FROM sent_client_ids WHERE sent_at < ?1", [cutoff])?
            + tx.execute("DELETE FROM client_adds WHERE created_at < ?1", [cutoff])?;

        // (user, folder) -> items some device holds.
        let mut held: HashMap<(String, String), HashSet<String>> = HashMap::new();
        {
            let mut rows = tx.prepare(
                "SELECT user, collection_id, snapshot, prev_snapshot FROM collection_sync",
            )?;
            let mut rows = rows.query([])?;
            while let Some(row) = rows.next()? {
                let items = held.entry((row.get(0)?, row.get(1)?)).or_default();
                for column in [2, 3] {
                    if let Some(json) = row.get::<_, Option<String>>(column)? {
                        let snapshot: Snapshot = serde_json::from_str(&json)?;
                        items.extend(snapshot.into_keys());
                    }
                }
            }
        }
        let candidates: Vec<(String, u32, String)> = tx
            .prepare("SELECT user, folder_id, email_id FROM item_ids WHERE created_at < ?1")?
            .query_map([before(retention.item_grace)], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        for (user, folder_id, email_id) in candidates {
            let is_held = held
                .get(&(user.clone(), folder_id.to_string()))
                .is_some_and(|items| items.contains(&email_id));
            if !is_held {
                pruned.item_ids += tx.execute(
                    "DELETE FROM item_ids WHERE user = ?1 AND folder_id = ?2 AND email_id = ?3",
                    params![user, folder_id, email_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(pruned)
    }
}
