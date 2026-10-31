//! The schema and its migrations, applied in order on open.

use super::*;

/// Schema migrations; entry `i` brings the database to version `i + 1`
/// (`PRAGMA user_version`). Never edit a released entry, append a new one.
/// Version 1 is idempotent so databases created before versioning (at 0)
/// upgrade cleanly.
pub(super) const MIGRATIONS: &[&str] = &[
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
];

pub(super) const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS devices (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    device_type TEXT,
    policy_key INTEGER,
    pending_policy_key INTEGER,
    info TEXT,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    PRIMARY KEY (user, device_id)
);

-- Short, stable EAS folder ids, shared by all devices of a user.
CREATE TABLE IF NOT EXISTS folder_ids (
    user TEXT NOT NULL,
    server_id INTEGER NOT NULL,
    mailbox_id TEXT NOT NULL,
    PRIMARY KEY (user, server_id),
    UNIQUE (user, mailbox_id)
);

-- The folder hierarchy each device has, for the current and previous
-- SyncKey (a client re-sends the previous key when a response got lost).
CREATE TABLE IF NOT EXISTS folder_sync (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    sync_key INTEGER NOT NULL,
    snapshot TEXT NOT NULL,
    prev_sync_key INTEGER,
    prev_snapshot TEXT,
    PRIMARY KEY (user, device_id)
);

-- Short, stable EAS item ids per folder, shared by all devices of a user.
CREATE TABLE IF NOT EXISTS item_ids (
    user TEXT NOT NULL,
    folder_id INTEGER NOT NULL,
    item_id INTEGER NOT NULL,
    email_id TEXT NOT NULL,
    PRIMARY KEY (user, folder_id, item_id),
    UNIQUE (user, folder_id, email_id)
);

-- Per device and collection: the items the device has (JSON snapshot) for
-- the current and previous SyncKey, plus the sticky Sync options.
CREATE TABLE IF NOT EXISTS collection_sync (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    sync_key INTEGER NOT NULL,
    snapshot TEXT NOT NULL,
    prev_sync_key INTEGER,
    prev_snapshot TEXT,
    options TEXT,
    PRIMARY KEY (user, device_id, collection_id)
);

-- ClientIds of messages a device sent, so a retried SendMail is not
-- delivered twice.
CREATE TABLE IF NOT EXISTS sent_client_ids (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    PRIMARY KEY (user, device_id, client_id)
);

-- The last Ping parameters (heartbeat and folders) per device; a Ping
-- without a body or without folders reuses them.
CREATE TABLE IF NOT EXISTS ping_params (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    params TEXT NOT NULL,
    PRIMARY KEY (user, device_id)
);

-- The server side of a collection (emails in the sync window with flags and
-- the JMAP Email state it reflects), refreshed from Email/changes.
CREATE TABLE IF NOT EXISTS collection_view (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    view TEXT NOT NULL,
    PRIMARY KEY (user, device_id, collection_id)
);
"#;

pub(super) const SCHEMA_V2: &str = r#"
-- Contacts and events a device created through Sync, by its ClientId, so a
-- retried Sync (the response got lost) does not create them twice.
CREATE TABLE client_adds (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    folder_id INTEGER NOT NULL,
    client_id TEXT NOT NULL,
    object_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user, device_id, folder_id, client_id)
);
"#;

pub(super) const SCHEMA_V3: &str = r#"
-- When an item id was assigned; unreferenced ids are pruned after a grace
-- period (0 for ids assigned before this column existed).
ALTER TABLE item_ids ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
"#;

pub(super) const SCHEMA_V4: &str = r#"
-- The last full Sync request per device (WBXML, without client commands):
-- an empty Sync repeats it and a partial Sync is merged into it.
CREATE TABLE sync_requests (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    request BLOB NOT NULL,
    PRIMARY KEY (user, device_id)
);
"#;

pub(super) const SCHEMA_V5: &str = r#"
-- Device management: whether a device may sync, or must wipe itself
-- (allowed, blocked, wipe, account-wipe, wiped), the login it last used and
-- its EAS version (141, 161, ...).
ALTER TABLE devices ADD COLUMN status TEXT NOT NULL DEFAULT 'allowed';
ALTER TABLE devices ADD COLUMN login TEXT;
ALTER TABLE devices ADD COLUMN version INTEGER;
"#;

pub(super) const SCHEMA_V6: &str = r#"
-- Per device and collection: how often in a row the device repeated the
-- SyncKey of a response it was sent (a lost response, or an item it
-- cannot take), whether it is probed one item at a time, and the items of
-- the last response while it is (JSON array).
CREATE TABLE sync_repeats (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    sync_key INTEGER NOT NULL,
    repeats INTEGER NOT NULL,
    last_at INTEGER NOT NULL,
    probing INTEGER NOT NULL,
    sent TEXT NOT NULL,
    PRIMARY KEY (user, device_id, collection_id)
);

-- Items a device kept failing on: skipped for that device as if sent.
CREATE TABLE quarantine (
    user TEXT NOT NULL,
    device_id TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    object_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user, device_id, collection_id, object_id)
);
"#;

pub(super) const SCHEMA_V7: &str = r#"
-- The last command that failed for a device, and when; and when each
-- collection's sync state last changed (for `devices show`).
ALTER TABLE devices ADD COLUMN last_error TEXT;
ALTER TABLE devices ADD COLUMN last_error_at INTEGER;
ALTER TABLE collection_sync ADD COLUMN updated_at INTEGER;
"#;

pub(super) const SCHEMA_V8: &str = r#"
-- The items of the response a device kept rejecting: only these are sent
-- one at a time while probing.
ALTER TABLE sync_repeats ADD COLUMN suspects TEXT NOT NULL DEFAULT '[]';
"#;

/// Brings the schema to the latest version, backing the database up first
/// (`<path>.v<old>-<time>.bak`) when it is a file that already holds data.
pub(super) fn migrate(conn: &mut Connection, path: Option<&Path>) -> anyhow::Result<()> {
    let latest = MIGRATIONS.len() as i64;
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > latest {
        anyhow::bail!(
            "database schema version {current} is newer than this build supports ({latest})"
        );
    }
    if current == latest {
        return Ok(());
    }
    let has_tables: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table'",
        [],
        |row| row.get(0),
    )?;
    if let Some(path) = path
        && has_tables
    {
        let backup = path.with_extension(format!("v{current}-{}.bak", unix_now()));
        conn.execute("VACUUM INTO ?1", [backup.to_string_lossy()])?;
        tracing::info!(backup = %backup.display(), "database backed up before migration");
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as i64 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "database migrated");
    }
    Ok(())
}
