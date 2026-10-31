//! Devices: their policy keys, information, status and logins, and moving
//! state stored under a login name to its account.

use super::*;

/// A device as `tolk devices list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub user: String,
    pub login: Option<String>,
    pub device_id: String,
    pub device_type: Option<String>,
    pub model: Option<String>,
    pub version: Option<u16>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub status: String,
    /// The last command that failed, and when.
    pub last_error: Option<(String, i64)>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DevicePolicy {
    /// The acknowledged key the device must send with every command.
    pub policy_key: Option<u32>,
    /// The temporary key handed out in the first Provision phase.
    pub pending_policy_key: Option<u32>,
}

impl Store {
    /// Moves state stored under a login name (as before state was keyed by
    /// JMAP account) to the account, once per login. Rows the account
    /// already has win. When the login's folder or item numbering clashes
    /// with the account's, its devices lose their sync state and start over
    /// (their SyncKeys are refused), rather than reading ids that now mean
    /// other folders or items.
    pub fn adopt_login(&self, login: &str, account: &str) -> anyhow::Result<()> {
        let key = (login.to_owned(), account.to_owned());
        if login == account || self.adopted.locked().contains(&key) {
            return Ok(());
        }
        let mut conn = self.conn.locked();
        let tx = conn.transaction()?;
        let devices: Vec<String> = tx
            .prepare("SELECT device_id FROM devices WHERE user = ?1")?
            .query_map([login], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut moved = 0;
        let mut clashes = 0;
        for table in USER_TABLES {
            moved += tx.execute(
                &format!("UPDATE OR IGNORE {table} SET user = ?2 WHERE user = ?1"),
                params![login, account],
            )?;
            let left = tx.execute(
                &format!("DELETE FROM {table} WHERE user = ?1"),
                params![login],
            )?;
            if matches!(*table, "folder_ids" | "item_ids") {
                clashes += left;
            }
        }
        if clashes > 0 {
            for device_id in &devices {
                for table in [
                    "folder_sync",
                    "collection_sync",
                    "collection_view",
                    "ping_params",
                ] {
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE user = ?1 AND device_id = ?2"),
                        params![account, device_id],
                    )?;
                }
            }
            tracing::warn!(
                login,
                account,
                clashes,
                devices = devices.len(),
                "login's ids clash with the account's, its devices start over"
            );
        }
        tx.commit()?;
        if moved > 0 {
            tracing::info!(
                login,
                account,
                rows = moved,
                "moved device state to the JMAP account"
            );
        }
        self.adopted.locked().insert(key);
        Ok(())
    }

    /// Records a failed command (shortened), for `devices show`.
    pub fn note_device_error(
        &self,
        user: &str,
        device_id: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        let error: String = error.chars().take(500).collect();
        self.conn.locked().execute(
            "UPDATE devices SET last_error = ?3, last_error_at = ?4
             WHERE user = ?1 AND device_id = ?2",
            params![user, device_id, error, unix_now()],
        )?;
        Ok(())
    }

    /// Records the login and EAS version a device used just now.
    pub fn note_device_login(
        &self,
        user: &str,
        device_id: &str,
        login: &str,
        version: u16,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "UPDATE devices SET login = ?3, version = ?4 WHERE user = ?1 AND device_id = ?2",
            params![user, device_id, login, version],
        )?;
        Ok(())
    }

    pub fn device_status(&self, user: &str, device_id: &str) -> anyhow::Result<String> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT status FROM devices WHERE user = ?1 AND device_id = ?2",
                params![user, device_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "allowed".to_owned()))
    }

    /// Sets a device's status; false when there is no such device.
    pub fn set_device_status(
        &self,
        user: &str,
        device_id: &str,
        status: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.conn.locked().execute(
            "UPDATE devices SET status = ?3 WHERE user = ?1 AND device_id = ?2",
            params![user, device_id, status],
        )? == 1)
    }

    /// Every known device, most recently seen first.
    pub fn list_devices(&self) -> anyhow::Result<Vec<DeviceRow>> {
        let conn = self.conn.locked();
        let mut statement = conn.prepare(
            "SELECT user, login, device_id, device_type, info, version, first_seen, last_seen, status,
                    last_error, last_error_at
             FROM devices ORDER BY last_seen DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                let info: Option<String> = row.get(4)?;
                let model = info
                    .and_then(|i| serde_json::from_str::<serde_json::Value>(&i).ok())
                    .and_then(|i| i["Model"].as_str().map(str::to_owned));
                Ok(DeviceRow {
                    user: row.get(0)?,
                    login: row.get(1)?,
                    device_id: row.get(2)?,
                    device_type: row.get(3)?,
                    model,
                    version: row.get(5)?,
                    first_seen: row.get(6)?,
                    last_seen: row.get(7)?,
                    status: row.get(8)?,
                    last_error: row.get::<_, Option<String>>(9)?.zip(row.get(10)?),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Forgets a device and all its state; it starts over if it returns.
    pub fn forget_device(&self, user: &str, device_id: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn.locked();
        let tx = conn.transaction()?;
        let mut removed = false;
        for table in DEVICE_TABLES {
            let n = tx.execute(
                &format!("DELETE FROM {table} WHERE user = ?1 AND device_id = ?2"),
                params![user, device_id],
            )?;
            removed |= *table == "devices" && n > 0;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Whether the account already has this device, and how many devices
    /// it has that are not wiped.
    pub fn device_count(&self, user: &str, device_id: &str) -> anyhow::Result<(bool, usize)> {
        let conn = self.conn.locked();
        let known: bool = conn.query_row(
            "SELECT count(*) > 0 FROM devices WHERE user = ?1 AND device_id = ?2",
            params![user, device_id],
            |row| row.get(0),
        )?;
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM devices WHERE user = ?1 AND status != 'wiped'",
            [user],
            |row| row.get(0),
        )?;
        Ok((known, count as usize))
    }

    pub fn touch_device(
        &self,
        user: &str,
        device_id: &str,
        device_type: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO devices (user, device_id, device_type, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT (user, device_id) DO UPDATE
             SET last_seen = ?4, device_type = COALESCE(?3, device_type)",
            params![user, device_id, device_type, unix_now()],
        )?;
        Ok(())
    }

    pub fn device_policy(
        &self,
        user: &str,
        device_id: &str,
    ) -> anyhow::Result<Option<DevicePolicy>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT policy_key, pending_policy_key FROM devices
                 WHERE user = ?1 AND device_id = ?2",
                params![user, device_id],
                |row| {
                    Ok(DevicePolicy {
                        policy_key: row.get(0)?,
                        pending_policy_key: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_pending_policy_key(
        &self,
        user: &str,
        device_id: &str,
        key: u32,
    ) -> anyhow::Result<()> {
        self.update_device(
            "UPDATE devices SET pending_policy_key = ?3 WHERE user = ?1 AND device_id = ?2",
            user,
            device_id,
            key,
        )
    }

    pub fn confirm_policy_key(&self, user: &str, device_id: &str, key: u32) -> anyhow::Result<()> {
        self.update_device(
            "UPDATE devices SET policy_key = ?3, pending_policy_key = NULL
             WHERE user = ?1 AND device_id = ?2",
            user,
            device_id,
            key,
        )
    }

    pub fn set_device_info(
        &self,
        user: &str,
        device_id: &str,
        info: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        self.update_device(
            "UPDATE devices SET info = ?3 WHERE user = ?1 AND device_id = ?2",
            user,
            device_id,
            serde_json::to_string(info)?,
        )
    }

    fn update_device(
        &self,
        sql: &str,
        user: &str,
        device_id: &str,
        value: impl rusqlite::ToSql,
    ) -> anyhow::Result<()> {
        let changed = self
            .conn
            .locked()
            .execute(sql, params![user, device_id, value])?;
        anyhow::ensure!(changed == 1, "unknown device {user}/{device_id}");
        Ok(())
    }
}
