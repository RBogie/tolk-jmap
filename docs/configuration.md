# Configuration and Stalwart reference

Tolk is configured with environment variables only. Invalid values stop
it at startup with an error naming the variable.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `JMAP_URL` | required | Base URL of the JMAP server, e.g. `https://mail.example.com`. The session is discovered at `/.well-known/jmap`. A plain `http://` URL works but logs a warning, since every request carries the user's password. |
| `LISTEN_ADDR` | `0.0.0.0:8080` | Address and port of the HTTP listener. The `healthcheck` subcommand uses the same port on 127.0.0.1. |
| `STATE_DB` | `state.sqlite` | SQLite database with device, folder and sync state. Back it up together with its `-wal` file, or stop the service first. Before a schema migration a copy is written next to it as `<file>.v<old>-<timestamp>.bak`. |
| `BACKUP_DIR` | `backups` next to `STATE_DB` | Where daily copies of the state database go (mode 0600). |
| `BACKUP_KEEP` | `7` | How many daily copies are kept; `0` turns backups off. |
| `TRUSTED_PROXIES` | empty | Comma separated addresses or CIDR ranges of reverse proxies (e.g. Traefik). Only from these peers are `X-Forwarded-For` and `X-Real-Ip` believed; the right-most untrusted hop is taken as the device address. With it empty, the TCP peer is the device address. |
| `EAS_VERSIONS` | `14.0,14.1,16.0,16.1` | EAS versions offered to devices. Devices pick one when the account is added and keep it; set `14.0,14.1` to keep new accounts on 14.1. |
| `PUBLIC_URL` | unset | The address devices reach Tolk at, e.g. `https://mail.example.com`, handed out by Autodiscover. `{domain}` stands for the domain of the address looked up, so `https://mail.{domain}` serves several mail domains. Unset: the host of each Autodiscover request (`Host`, or `X-Forwarded-Host` from a trusted proxy). Set it in production. |
| `AUTH_CACHE_TTL_SECS` | `300` | How long a verified JMAP session is reused before credentials are checked against the server again. A 401 from the server drops the cached session at once. |
| `TRAFFIC_DIR` | unset | Directory for raw EAS traffic captures. Unset disables capture entirely. |
| `TRAFFIC_CAPTURE` | empty | Whose traffic is written: comma separated `user`, `user/deviceid` or `*`. Empty captures nothing even when `TRAFFIC_DIR` is set. |
| `TRAFFIC_RETENTION_HOURS` | unset | How long capture files are kept. Takes precedence over `TRAFFIC_RETENTION_DAYS`. |
| `TRAFFIC_RETENTION_DAYS` | `1` | How long capture files are kept when `TRAFFIC_RETENTION_HOURS` is unset. Pruning runs hourly. |
| `MAX_REQUEST_MB` | `64` | Largest request body. Commands other than SendMail, SmartReply, SmartForward, Sync and ItemOperations are limited to 4 MiB regardless. |
| `SHUTDOWN_DEADLINE_SECS` | `8` | How long in-flight requests may finish after SIGTERM. Keep it below the container's stop timeout (`stop_grace_period`, 10 s by default). |
| `MAX_PINGS` | `1000` | Pings and waiting (hanging) Syncs at once; over it such a request gets 503 with Retry-After. |
| `MAX_COMMANDS` | `256` | Other requests running at once. |
| `LOGIN_FAILURES_PER_ADDRESS` | `10` | Failed logins from one address within 15 minutes before new logins from there are refused without asking the JMAP server. A password that worked before for that user still passes. |
| `LOGIN_FAILURES_PER_USER` | `20` | The same for one user from any address. |
| `METRICS_ADDR` | unset | Address for Prometheus metrics (`/metrics`), e.g. `0.0.0.0:9090`: requests per command and status with their time, failed logins, waiting Pings, running commands and state table sizes. Keep it off the public route. |
| `MAX_DEVICES_PER_USER` | `20` | Devices one account may register; a new one beyond it gets HTTP 403 until an old one is forgotten (`devices forget`). Wiped devices do not count. |
| `SYNC_MAX_AGE_DAYS` | unset | How many days back any device may sync mail and calendars. A device's sync window (FilterType) is shortened to it, and "all" counts as this many days. Unset: no limit, devices choose. |
| `CONTACT_PICTURE_MAX_KB` | `48` | Contact photos larger than this (as Exchange limits them) are not sent to devices; they stay on the server, and edits from the device leave them alone. |
| `SEARCH_MAX_RESULTS` | `100` | Most results one Search request returns (mailbox and GAL). |
| `SHARED_POLL_SECS` | `120` | How often a waiting Ping or Sync re-checks folders in shared accounts (shared mailboxes, calendars and address books), since Stalwart pushes no changes for them. Only devices watching a shared folder poll this often, and only the shared folders are checked; at least 5. |
| `RUST_LOG` | `info,tolk=debug` | Log filter in `tracing` syntax. At `info` logs name users, devices and commands but no message content; `debug` adds JMAP requests and responses, which can contain event and contact details. |
| `LOG_FORMAT` | text | `json` writes one JSON object per line. Every line of a request carries its `id`, `user`, `device` and `cmd`. |

Traffic captures contain complete messages and attachments. Files are written
with mode 0600 in a 0700 directory; enable capture only for test accounts or
while debugging a specific device.

### Fixed limits

These are constants in the code:

| Limit | Value | Where |
|---|---|---|
| Request elements | 100,000 per request, decoded text at most twice the request size plus 1 MiB; DeviceId and DeviceType up to 128 printable characters | `src/wbxml/decode.rs`, `src/limits.rs` |
| Command run time | 5 minutes; Ping and Sync their longest wait plus 5 minutes | `src/limits.rs` |
| Ping heartbeat | 60 to 3540 s (as Exchange), up to 300 folders | `src/eas/ping.rs` |
| Ping storm detection | 10 immediate answers in 60 s delay the next one by 30 s | `src/push.rs` |
| Sync window | 100 by default, 512 at most | `src/eas/sync/mod.rs` |
| Sync loop detection | the same SyncKey 3 times in a row, each within 30 min, sends the items of that response one at a time; one the device repeats 3 times too is quarantined | `src/eas/sync/mod.rs` |
| Recipient lookup cache | 15 min | `src/jmap/people.rs` |
| Push reconnect delay | 10 s | `src/push.rs` |

## Device management

Run these in the container (`docker exec tolk tolk devices ...`);
they change the state database next to the running service.

| Command | Effect |
|---|---|
| `devices list` | Every device: user, device id, type, model, EAS version, status, last seen. |
| `devices block <user> <device>` | The device gets HTTP 403 from now on. `unblock` undoes it. |
| `devices wipe <user> <device>` | Account-only wipe (EAS 16.1 devices): the device removes this account and its data at its next connection, then is refused. |
| `devices wipe <user> <device> --full` | Full remote wipe: the device erases itself (factory reset). Irreversible. |
| `devices forget <user> <device>` | Drops the device and its sync state; if it connects again it starts over. |
| `devices show <user> <device>` | Its FolderSync key, collections (SyncKey, items, last change, options), Ping parameters, quarantined items and last error. |
| `devices resync <user> <device> [<collection>]` | That collection, or the folder hierarchy and every collection, start over: the device gets "invalid SyncKey" and resyncs by itself. |
| `devices clear-quarantine <user> <device>` | Sends the items quarantined for the device again. |

`<user>` is the login the device uses (or its JMAP account id).

## Backups and restore

The state database holds only sync bookkeeping; mail, contacts and
calendars live on the JMAP server. Losing it costs a full resync of every
device, not data. A copy is made once a day (`BACKUP_DIR`, `BACKUP_KEEP`)
while the service runs.

To restore: stop the container, copy a backup over `STATE_DB` (and remove
`STATE_DB-wal` and `STATE_DB-shm`), start it again. Devices whose SyncKeys
are newer than the backup get "invalid SyncKey" and resync those folders
from scratch; nothing is duplicated or lost on the server. To make every
device start over, delete the database instead.

## Endpoints

| Path | Purpose |
|---|---|
| `/Microsoft-Server-ActiveSync` | The EAS endpoint. Route it (and only it) from the reverse proxy. |
| `/autodiscover/autodiscover.xml` | Autodiscover for mobile clients (any letter case, POST, Basic auth checked against JMAP). Only the `mobilesync` response schema is offered; Outlook desktop schemas get error 601. |
| `/autodiscover/autodiscover.json/v1.0/<address>?Protocol=ActiveSync` | The v2 JSON form (unauthenticated), naming the EAS URL. |
| `/healthz` | 200 while the process serves requests. Used by the image's `HEALTHCHECK`. |
| `/readyz` | 200 when the state database answers and the JMAP server is reachable, 503 with the reason otherwise. |

`tolk --version` prints the crate version and, for images built with
`--build-arg GIT_SHA=<commit>`, the git commit.

### Autodiscover routing

Clients look for Autodiscover at, in order, `https://<domain>/autodiscover/autodiscover.xml`,
`https://autodiscover.<domain>/autodiscover/autodiscover.xml` and the
`_autodiscover._tcp.<domain>` SRV record, where `<domain>` is the part of the
address after `@`. To make account setup work with only an address and a
password, route one of these to Tolk for every mail domain:

- a reverse proxy rule for the path `/autodiscover/` (any case) on the
  domain's web host or on `autodiscover.<domain>` (with a certificate for
  that name), or
- an SRV record `_autodiscover._tcp.<domain>` pointing at the host that
  already serves `/Microsoft-Server-ActiveSync` (port 443), plus the path
  rule on that host.

Set `PUBLIC_URL` when the Autodiscover host is not the host that serves
EAS. Without Autodiscover, clients still work when the user enters the
server name by hand.

## What the JMAP server must offer

Tolk is developed against Stalwart. Any JMAP server with the same
capabilities should work, but has not been tried.

| Capability | Needed for |
|---|---|
| `urn:ietf:params:jmap:core` | Everything. |
| `urn:ietf:params:jmap:mail` | Required: mail folders, Sync, Search, ItemOperations, MoveItems. |
| `urn:ietf:params:jmap:submission` | SendMail, SmartReply, SmartForward, meeting responses. |
| `urn:ietf:params:jmap:contacts` | Optional: the contacts folder and recipient lookup from contacts. Without it, no contacts folder is offered. |
| `urn:ietf:params:jmap:calendars` | Optional: calendar folders, event editing and invitations. |
| `urn:ietf:params:jmap:calendars:parse` | Showing and answering invitations that arrive by mail (`CalendarEvent/parse`). |
| `urn:ietf:params:jmap:principals` | Looking up the user's own calendar address. Directory queries are not needed (Stalwart forbids them). |

Push uses the session's `eventSourceUrl` for the types Email, Mailbox,
ContactCard, AddressBook, CalendarEvent and Calendar; changes the server
reports for shared accounts count too (Stalwart reports none, so shared
calendars and address books are rechecked every 2 minutes while a Ping or
Sync waits). If the stream drops, Ping falls back to polling every 2
minutes until it reconnects.

### Trust and network

- Tolk authenticates every JMAP request with the device's own
  credentials (HTTP Basic, app passwords work). It stores no passwords; the
  session cache keys on a salted hash.
- OAuth: a device may send `Authorization: Bearer <access token>` instead,
  with a token the JMAP server issued (Stalwart is an OAuth provider; its
  metadata is at `/.well-known/oauth-authorization-server`). The token is
  passed on to JMAP as it came, so it works wherever it works on JMAP, push
  included. When it expires, requests get 401 and the device must bring a
  new one. Mainstream phone mail apps only do OAuth for Exchange against
  Microsoft's own login service, so in practice this serves clients and
  tools that let you supply a token (`eas-client --token-file`); phones keep
  using passwords or app passwords. Tolk only offers Basic in its
  401 challenge, so Outlook is not sent looking for a Microsoft login.
- Every JMAP request carries the device address in `X-Forwarded-For`, so the
  JMAP server's rate limiting and fail2ban act on the phone instead of on
  Tolk. Configure the JMAP server to trust that header from the
  Tolk host (in Stalwart, the HTTP listener's forwarded-header and
  trusted-network settings). Without that, a few failed
  logins from one phone can block every user behind Tolk.
- The Tolk host must be allowed through any firewall in front of the
  JMAP server on port 443.

### JSCalendar 2.0 notes

Stalwart implements the JSCalendar 2.0 draft, not RFC 8984. Tolk writes
what Stalwart reads:

- A single `recurrenceRule` object instead of the `recurrenceRules` array
  (the array is cleared on write, since Stalwart ignores it).
- Participants carry `calendarAddress` (a `mailto:` URI) instead of `email`
  and `sendTo`, and the organizer is identified by
  `organizerCalendarAddress`.
- Scheduling messages for invitations are sent by the server when
  `sendSchedulingMessages` is true on `CalendarEvent/set`.

Contacts use JSContact (RFC 9553) through `ContactCard`.
