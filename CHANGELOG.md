# Changelog

## Unreleased
- 

## 0.1.0 (2026-10-31)

First release. Developed against Stalwart 1.0 with Gmail on Android;
also tried against Cyrus.

### Protocol

- EAS 14.0, 14.1, 16.0 and 16.1 over WBXML: Provision, FolderSync and
  folder operations, Sync (hanging, empty and partial), GetItemEstimate,
  Ping, ItemOperations (fetch, empty folder), MoveItems, SendMail,
  SmartReply, SmartForward, Search, Find (16.1), MeetingResponse,
  Settings (out-of-office) and ResolveRecipients.
- Autodiscover (XML and the v2 JSON form); other schemas are passed on to
  the JMAP server.
- Push from the JMAP EventSource stream, re-checking only what changed.

### Mail, contacts and calendars

- Mail: folders, sync windows, read, flag and answered state, moves and
  deletes, attachments (streamed to the device as they download), search,
  replies and forwards, and drafts with attachments (16.x).
- Contacts (JSContact) both ways. Edits only write the fields the device
  changed, so entries beyond the EAS slots survive; photos stored as blobs
  are sent too.
- Calendars (JSCalendar) both ways: recurrence and exceptions, single
  occurrences by InstanceId (16.x), invitations and responses.
- Shared mailboxes, calendars and address books from other accounts in the
  JMAP session, as folders named "<name> (<owner>)". Read-only shares
  refuse changes; servers that push no changes for shared accounts are
  polled (`SHARED_POLL_SECS`).

### Distribution

- A fully static binary for linux/amd64, arm64, arm/v7, 386, ppc64le and
  riscv64, and a multi-platform image on ghcr.io that contains only that
  binary and CA certificates. Both carry signed build provenance; the image
  also has an SBOM.

### Operations

- Logins with passwords or OAuth bearer tokens issued by the JMAP server.
- Device management: list, show, block, account-only or full wipe,
  resync, forget, clear quarantine.
- Sync loop detection: an item a device keeps rejecting is found and
  skipped for that device.
- State in SQLite with schema migrations, backups before each migration
  and daily, idempotent retries and pruning of stale state.
- Health and readiness endpoints, Prometheus metrics, request-scoped and
  JSON logs, login failure limits, request size and time limits, and
  configurable caps (`SYNC_MAX_AGE_DAYS`, `CONTACT_PICTURE_MAX_KB`,
  `SEARCH_MAX_RESULTS`).

### Testing

- Unit and property tests, an in-process fake JMAP server with end-to-end
  flows, sanitized session replay, fuzz targets, and scenarios for a
  scripted EAS client (`eas-client`).
