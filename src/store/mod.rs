//! Per-user, per-device EAS state in SQLite.
//!
//! EAS is client-state driven: the server has to remember what each device
//! was sent. JMAP state strings are per account, so they cannot do that alone.

use crate::LockExt;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

/// Tables with state per device, dropped when a device is retired.
const DEVICE_TABLES: &[&str] = &[
    "folder_sync",
    "sync_requests",
    "collection_sync",
    "sent_client_ids",
    "ping_params",
    "collection_view",
    "client_adds",
    "sync_repeats",
    "quarantine",
    "devices",
];

pub struct Store {
    conn: Mutex<Connection>,
    /// Logins whose state was already moved to their account (see
    /// [`Store::adopt_login`]).
    adopted: Mutex<HashSet<(String, String)>>,
}

/// Every table with per-user state.
const USER_TABLES: &[&str] = &[
    "devices",
    "folder_ids",
    "folder_sync",
    "item_ids",
    "collection_sync",
    "sent_client_ids",
    "ping_params",
    "collection_view",
    "client_adds",
    "sync_requests",
    "sync_repeats",
    "quarantine",
];

mod devices;
mod ids;
mod pruning;
mod quarantine;
mod schema;
mod sync_state;

pub use devices::{DevicePolicy, DeviceRow};
pub use pruning::{Pruned, Retention};
pub use quarantine::{QuarantinedItem, SyncRepeats};
use schema::migrate;
pub use sync_state::{CollectionSummary, CollectionSyncState, FolderSyncState};

/// JMAP id to version, for every item a device has (see `eas::sync`).
pub type Snapshot = BTreeMap<String, u64>;

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn, Some(path))?;
        Ok(Self {
            conn: Mutex::new(conn),
            adopted: Mutex::default(),
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        migrate(&mut conn, None)?;
        Ok(Self {
            conn: Mutex::new(conn),
            adopted: Mutex::default(),
        })
    }

    /// Row count per table, for the maintenance log.
    pub fn row_counts(&self) -> anyhow::Result<Vec<(&'static str, i64)>> {
        let conn = self.conn.locked();
        USER_TABLES
            .iter()
            .map(|table| {
                let count =
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                Ok((*table, count))
            })
            .collect()
    }

    /// Writes a consistent copy of the database to `dest` (which must not
    /// exist), while the service keeps running.
    pub fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        self.conn
            .locked()
            .execute("VACUUM INTO ?1", [dest.to_string_lossy()])
            .with_context(|| format!("back up to {}", dest.display()))?;
        Ok(())
    }

    /// Whether the database answers (readiness check).
    pub fn check(&self) -> anyhow::Result<()> {
        self.conn
            .locked()
            .query_row("SELECT count(*) FROM devices", [], |_| Ok(()))?;
        Ok(())
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::schema::{MIGRATIONS, SCHEMA_V1};
    use super::*;

    #[test]
    fn migrates_once_and_refuses_newer_databases() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, None).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        migrate(&mut conn, None).unwrap();

        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(migrate(&mut conn, None).is_err());
    }

    #[test]
    fn upgrades_unversioned_databases_with_a_backup() {
        let dir = std::env::temp_dir().join(format!("tolk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.sqlite");
        let _ = std::fs::remove_file(&path);
        {
            // A database from before versioning: tables, user_version 0.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute(
                "INSERT INTO devices (user, device_id, first_seen, last_seen) VALUES ('rob', 'd1', 1, 1)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.device_policy("rob", "d1").unwrap().is_some());
        let backups = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".bak")
            })
            .count();
        assert_eq!(backups, 1);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adopts_state_stored_under_a_login() {
        let store = Store::open_in_memory().unwrap();
        store.touch_device("rob", "d1", None).unwrap();
        store.touch_device("rob", "d2", None).unwrap();
        store.item_ids("rob", 7, &["m1"]).unwrap();
        // The account already has state for d2: that row wins.
        store.touch_device("acc", "d2", Some("Phone")).unwrap();

        store.adopt_login("rob", "acc").unwrap();
        let conn = store.conn.locked();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM devices WHERE user = 'rob'"), 0);
        assert_eq!(count("SELECT count(*) FROM devices WHERE user = 'acc'"), 2);
        assert_eq!(
            count("SELECT count(*) FROM devices WHERE user = 'acc' AND device_type = 'Phone'"),
            1
        );
        assert_eq!(
            count("SELECT item_id FROM item_ids WHERE user = 'acc' AND email_id = 'm1'"),
            1
        );
    }

    #[test]
    fn prunes_idle_devices_old_client_ids_and_unheld_items() {
        let store = Store::open_in_memory().unwrap();
        let snapshot = |items: &str| CollectionSyncState {
            sync_key: 2,
            snapshot: items.to_owned(),
            previous: Some((1, r#"{"prev":1}"#.to_owned())),
            options: None,
        };
        store.touch_device("rob", "old", None).unwrap();
        store.touch_device("rob", "new", None).unwrap();
        store
            .save_collection_sync("rob", "old", "7", &snapshot(r#"{"a":1}"#))
            .unwrap();
        store
            .save_collection_sync("rob", "new", "7", &snapshot(r#"{"b":1}"#))
            .unwrap();
        store.claim_client_id("rob", "new", "c1").unwrap();
        store
            .item_ids("rob", 7, &["a", "b", "prev", "gone"])
            .unwrap();
        {
            let conn = store.conn.locked();
            conn.execute(
                "UPDATE devices SET last_seen = 0 WHERE device_id = 'old'",
                [],
            )
            .unwrap();
            conn.execute("UPDATE sent_client_ids SET sent_at = 0", [])
                .unwrap();
            conn.execute(
                "UPDATE item_ids SET created_at = 0 WHERE email_id != 'b'",
                [],
            )
            .unwrap();
        }

        let pruned = store.prune(&Retention::default()).unwrap();
        assert_eq!(
            pruned,
            Pruned {
                devices: 1,
                client_ids: 1,
                // "a" was only held by the retired device; "gone" by nobody.
                item_ids: 2,
            }
        );
        assert!(
            store
                .collection_sync_state("rob", "old", "7")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .collection_sync_state("rob", "new", "7")
                .unwrap()
                .is_some()
        );
        assert!(
            store.claim_client_id("rob", "new", "c1").unwrap(),
            "ClientId forgotten"
        );
        // Held ("prev" in a previous snapshot) and recent ("b") ids stay.
        let ids = store.item_ids("rob", 7, &["b", "prev"]).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            store.prune(&Retention::default()).unwrap(),
            Pruned::default()
        );
    }

    #[test]
    fn device_policy_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.device_policy("rob", "d1").unwrap(), None);
        store.touch_device("rob", "d1", Some("Android")).unwrap();
        assert_eq!(
            store.device_policy("rob", "d1").unwrap(),
            Some(DevicePolicy::default())
        );

        store.set_pending_policy_key("rob", "d1", 7).unwrap();
        store.confirm_policy_key("rob", "d1", 9).unwrap();
        let policy = store.device_policy("rob", "d1").unwrap().unwrap();
        assert_eq!(
            (policy.policy_key, policy.pending_policy_key),
            (Some(9), None)
        );

        assert!(store.set_pending_policy_key("rob", "unknown", 1).is_err());
    }

    #[test]
    fn folder_ids_are_stable_per_user() {
        let store = Store::open_in_memory().unwrap();
        let first = store.folder_server_ids("rob", &["a", "b"]).unwrap();
        let second = store.folder_server_ids("rob", &["b", "c", "a"]).unwrap();
        assert_eq!(first["a"], second["a"]);
        assert_eq!(first["b"], second["b"]);
        assert_eq!(second["c"], 3);
        assert_eq!(store.folder_server_ids("eva", &["a"]).unwrap()["a"], 1);
    }

    #[test]
    fn item_ids_are_stable_per_folder() {
        let store = Store::open_in_memory().unwrap();
        let first = store.item_ids("rob", 7, &["m1", "m2"]).unwrap();
        let again = store.item_ids("rob", 7, &["m2", "m3"]).unwrap();
        assert_eq!(first["m2"], again["m2"]);
        assert_eq!(again["m3"], 3);
        assert_eq!(store.item_ids("rob", 8, &["m1"]).unwrap()["m1"], 1);
        assert_eq!(
            store
                .email_for_item("rob", 7, again["m3"])
                .unwrap()
                .as_deref(),
            Some("m3")
        );
        assert_eq!(store.email_for_item("rob", 7, 99).unwrap(), None);
    }

    #[test]
    fn client_ids_are_claimed_once() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.claim_client_id("rob", "d1", "c1").unwrap());
        assert!(!store.claim_client_id("rob", "d1", "c1").unwrap());
        assert!(store.claim_client_id("rob", "d2", "c1").unwrap());
        store.release_client_id("rob", "d1", "c1").unwrap();
        assert!(store.claim_client_id("rob", "d1", "c1").unwrap());
    }

    #[test]
    fn reset_drops_collection_state_only_for_that_device() {
        let store = Store::open_in_memory().unwrap();
        let state = CollectionSyncState {
            sync_key: 1,
            snapshot: "{}".to_owned(),
            previous: None,
            options: None,
        };
        store
            .save_collection_sync("rob", "d1", "7", &state)
            .unwrap();
        store
            .save_collection_sync("rob", "d2", "7", &state)
            .unwrap();
        store.save_ping_params("rob", "d1", "p").unwrap();
        store.reset_collections("rob", "d1").unwrap();
        assert_eq!(store.collection_sync_state("rob", "d1", "7").unwrap(), None);
        assert_eq!(store.ping_params("rob", "d1").unwrap(), None);
        assert!(
            store
                .collection_sync_state("rob", "d2", "7")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ping_params_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.ping_params("rob", "d1").unwrap(), None);
        store.save_ping_params("rob", "d1", "a").unwrap();
        store.save_ping_params("rob", "d1", "b").unwrap();
        assert_eq!(
            store.ping_params("rob", "d1").unwrap().as_deref(),
            Some("b")
        );
    }

    #[test]
    fn collection_view_round_trips() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.collection_view("rob", "d1", "7").unwrap(), None);
        store.save_collection_view("rob", "d1", "7", "a").unwrap();
        store.save_collection_view("rob", "d1", "7", "b").unwrap();
        assert_eq!(
            store.collection_view("rob", "d1", "7").unwrap().as_deref(),
            Some("b")
        );
    }

    #[test]
    fn collection_sync_state_round_trips() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.collection_sync_state("rob", "d1", "7").unwrap(), None);
        let state = CollectionSyncState {
            sync_key: 2,
            snapshot: "{}".to_owned(),
            previous: Some((1, "{\"a\":0}".to_owned())),
            options: Some("{}".to_owned()),
        };
        store
            .save_collection_sync("rob", "d1", "7", &state)
            .unwrap();
        assert_eq!(
            store.collection_sync_state("rob", "d1", "7").unwrap(),
            Some(state)
        );
    }

    #[test]
    fn folder_sync_state_round_trips() {
        let store = Store::open_in_memory().unwrap();
        store.save_folder_sync("rob", "d1", None, 1, "[]").unwrap();
        store
            .save_folder_sync("rob", "d1", Some((1, "[]")), 2, "[1]")
            .unwrap();
        let state = store.folder_sync_state("rob", "d1").unwrap().unwrap();
        assert_eq!(state.sync_key, 2);
        assert_eq!(state.snapshot, "[1]");
        assert_eq!(state.previous, Some((1, "[]".to_owned())));
    }
}
