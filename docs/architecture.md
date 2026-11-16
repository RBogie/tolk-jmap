# Architecture

How Tolk is put together, for contributors. The module docs (`//!` at the
top of each file) go into more detail; this page is the map.

## The problem

Exchange ActiveSync (EAS) is client-state driven: the device sends a
SyncKey, and the server must know exactly what it sent the device under that
key. JMAP is server-state driven: a client asks "what changed since state
X" and keeps its own copy. Tolk sits between them. It translates each EAS
command into JMAP calls made with the device's own credentials, and keeps
the per-device bookkeeping EAS needs and JMAP does not provide. It keeps no
mail.

```text
 phone ──HTTPS──▶ reverse proxy ──HTTP──▶ Tolk ──HTTPS──▶ JMAP server
 (EAS, WBXML)     (TLS, routing)          │               (mail, contacts,
                                          │                calendars, push)
                                          ▼
                                   SQLite state
                          (devices, ids, sync snapshots)
```

## A request

`src/eas/mod.rs` handles `/Microsoft-Server-ActiveSync`:

1. **Credentials** (`auth.rs`): HTTP Basic or an OAuth bearer token. Failed
   logins are counted per address and per user (`limits.rs`), and an
   address over the limit is refused before the JMAP server is asked.
   Tokens are passed through; with `OAUTH_ISSUER` (experimental), `oauth.rs`
   tells clients where to get one.
2. **JMAP session** (`jmap/mod.rs`, `JmapPool`): the credentials open a
   JMAP session, which also verifies them. Sessions are cached for
   `AUTH_CACHE_TTL_SECS` under a keyed hash of the password. With
   `JMAP_SERVERS`, the login's mail domain picks the server.
3. **Body**: only read once the login is good, under a size limit, and
   decoded from WBXML (`wbxml/`) into an `Element` tree.
4. **Device**: state is keyed by the JMAP account (`JmapClient::user_key`),
   not the login name, so `rob` and `rob@example.com` share it. One command
   runs at a time per device (Ping excepted); new, blocked and wiped
   devices are handled; data commands need a completed Provision.
5. **EAS 16**: requests are translated to 14.1 shapes on the way in and
   back on the way out (`eas/v16.rs`), so the command handlers deal with
   one shape.
6. **Command**: dispatched to its module in `src/eas/`.
7. **Response**: encoded to WBXML. Attachments and whole MIME messages are
   streamed into the response as they download from JMAP (`eas/stream.rs`),
   so large files are never held in memory.
8. **Capture** (`traffic.rs`): for users listed in `TRAFFIC_CAPTURE`, the
   request and response are written to disk for debugging and fixtures.

## State

`src/store/` keeps everything in one SQLite database (`STATE_DB`):

| Table | What it holds |
|---|---|
| `devices` | Device id, type, policy key, EAS version, status (blocked, wipe), logins. |
| `folder_ids`, `item_ids` | Short EAS ids for JMAP mailboxes, address books, calendars and items. EAS ids are small numbers; JMAP ids are opaque strings. |
| `folder_sync` | Per device: the folder hierarchy snapshot for the current and previous FolderSync key. |
| `collection_sync` | Per device and folder: the snapshot of items (id and version) sent under the current SyncKey, and the previous one. |
| `collection_view` | Per device and folder: the server side as last seen, with the JMAP state it reflects. |
| `sync_requests` | The last full Sync request, which empty and partial Syncs repeat. |
| `ping_params` | The folders and heartbeat of the last Ping, which an empty Ping repeats. |
| `sent_client_ids`, `client_adds` | Sends (ClientIds and drafts) with the message stored for them and whether they went out, and the ClientIds of device Adds, so retries do not send or create twice (`eas/outbox.rs`). |
| `sync_repeats`, `quarantine` | Sync loops: a device repeating a SyncKey, and items it keeps failing on. |

Schema changes are migrations in `store/schema.rs`, applied on startup after
a copy of the database is saved. `maintenance.rs` prunes idle devices and
old ids hourly and makes a daily backup.

## Sync

The core of Tolk is `eas/sync/` with `eas/view.rs`, the same for mail,
contacts and calendars:

- **View**: the server side of a folder, a list of item ids with a version
  each. It is built once by listing the folder, then caught up with
  `<type>/changes`, so an idle poll is one small JMAP call. A version is a
  hash of what the device would receive, so a server change the device
  cannot see (a keyword EAS has no field for) is not sent.
- **Snapshot**: what the device holds under its SyncKey. A Sync diffs the
  view against the snapshot (`Plan`), sends at most WindowSize changes, and
  stores the advanced snapshot under a new key. The previous snapshot is
  kept, so a device that lost the response and repeats its key gets the
  same batch again.
- **Device changes**: the device's own Adds, Changes and Deletes are applied
  to JMAP first and to the snapshot, so they are not echoed back. Contacts
  and calendars share `device_changes.rs`; mail flags, moves and drafts
  live in `sync/mail.rs` and `drafts.rs`.
- **Quarantine**: a device that keeps repeating a SyncKey is choking on
  something. Tolk narrows down which item and leaves it out for that
  device.

FolderSync (`folder_sync.rs`) works the same way, with the full mailbox
list as the view: folder counts are small.

## Data mapping

| EAS | JMAP | Module |
|---|---|---|
| Email | `Email` (RFC 8621) | `eas/email.rs`, `jmap/mail.rs` |
| Contacts | `ContactCard`, JSContact (RFC 9553) | `eas/jscontact/` |
| Calendar | `CalendarEvent`, JSCalendar (RFC 8984, 2.0 recurrence) | `eas/jscalendar.rs`, `eas/tz.rs` |
| SendMail, SmartReply, SmartForward | `Email/import` and `EmailSubmission/set` | `eas/compose.rs` |
| Out of office | `VacationResponse` | `eas/settings.rs` |
| GAL search, ResolveRecipients | address books, recent correspondents, principals | `jmap/people.rs` |

EAS has fixed slots (three email addresses, one home address, and so on).
Contact edits from the device only write the fields it changed
(`jscontact/merge.rs`), so data beyond those slots survives a round trip
through a phone.

## Push

EAS clients get push from Ping (and from Syncs that wait, used by iOS): a
long-poll that returns when a watched folder changes. `push.rs` follows the
account's JMAP EventSource stream, one task per account shared by all its
waiting requests, and wakes them on a state change of the right kind.
Folders in shared accounts, which the stream does not cover, are polled
every `SHARED_POLL_SECS`.

## Shared accounts

Mailboxes, address books and calendars other users share appear as folders
of the device. A folder remembers its JMAP account, and the command handlers
use `JmapClient::in_account` for it. Read-only shares refuse device changes.

## Several JMAP servers

`JMAP_URL` is the default server; `JMAP_SERVERS` adds servers per mail
domain. Each server has its own caches, and the state of its accounts is
stored under `<host>/<account id>`, since account ids of different servers
can be the same.

## Tests

- **Unit tests** next to the code, including property tests of the WBXML
  codec (`wbxml/proptests.rs`).
- **Integration tests** (`tests/eas_flows.rs`) drive the real router with an
  EAS client (`client.rs`) against an in-process fake JMAP server
  (`tests/support/fake_jmap.rs`).
- **Replay tests** (`tests/replay.rs`) run sanitized sessions recorded from
  real devices (`tests/fixtures/sessions/`).
- **Fuzzing** (`fuzz/`): the WBXML decoder, the query string parser and the
  time zone blob, briefly on every pull request.
- **Scenarios** (`scenarios/`) run the scripted `eas-client` against real
  JMAP servers: in CI against Stalwart 1.0, Stalwart 0.16 and Cyrus.
