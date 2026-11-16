# Changelog

## Unreleased

### Fixed

- Conflicting changes (MS-ASCMD 2.2.3.34): a Change for a contact, event
  or message (read and flag state) that the server changed since the device
  last received it no longer overwrites the server's version. The device gets status 7 and the
  server's version, unless it asked for `<Conflict>0</Conflict>`. A Delete
  still wins.
- Editing an event on a device only writes what it changed. A field sent
  back as the device was shown it stays as it is, so an edit on the phone
  no longer turns several reminders or locations into one, `secret` into
  `private`, a recurrence rule EAS cannot express (EAS 14) into a single
  event, or the event's time zone into another name with the same rules.
  A truncated description sent back unchanged is no longer cut either.
- Cancelled meetings reach the device as cancelled (MeetingStatus 5 or 7).
- Attachment and message downloads to the device are no longer cut off
  after 30 seconds on a slow connection: they only fail when the JMAP
  server sends nothing for 90 seconds. Other JMAP calls keep their
  30-second limit.
- SmartReply and SmartForward with `ReplaceMime` (as iOS sends them) mark
  the original as answered or forwarded.
- ItemOperations Range fetches are streamed to the device like whole
  attachments instead of read into memory first. Only when the JMAP server
  gives no lengths is the part read first, and then at most 10 MiB (the
  response's Range says what was sent). A Range ending at the largest
  number no longer makes the request fail.
- docs: compatibility.md no longer says remote wipe is not offered.
- Failed sends:
  - A message the JMAP server refuses no longer leaves a copy in Sent.
  - When the server's answer to a send is lost, the device is told to try
    again later, and its retry checks whether the message went out instead
    of sending it a second time.
  - A draft sent with a Sync Add (EAS 16) that fails is reported to the
    device instead of as sent, and the device's retry sends it. A repeated
    draft send that already went out is not sent again.
  - A refusal for missing or invalid recipients gets its own status
    (SendMail 119 or 116, a draft send 183) instead of a general failure.
- The warning for a login without a JMAP mail account no longer blames
  OAuth when the login was a password (for example an app password limited
  to IMAP and SMTP).
- Device changes follow MS-ASCMD on fields the device leaves out:
  - Calendar: a left-out description (iOS leaves it out when only the time
    changes) or `<Exceptions>` stays as it is, and so does a single
    occurrence left out of `<Exceptions>`. With EAS 16, every left-out field
    stays; with EAS 14, `<Supported>` now also applies to calendars.
    Occurrences keep what EAS cannot show, also when one is changed by
    InstanceId.
  - Contacts: a left-out body no longer deletes the notes.
  - Drafts (EAS 16): a left-out To, Cc or Subject is removed instead of
    kept, and an edited reply keeps its `In-Reply-To` and `References`.

## 0.2.0 (2026-11-15)

### Added

- `JMAP_SERVERS`: one Tolk in front of several JMAP servers, chosen by the
  mail domain of the login.
- `OAUTH_ISSUER` (experimental): advertises where clients get an OAuth token
  (RFC 9728 protected resource metadata and a `Bearer` challenge). Off by
  default; it may change or go away in any release.

### Changed

- Whole messages (MIME bodies) fetched with ItemOperations are streamed to
  the device as they download, like attachments already were.
- Ranged attachment fetches ask the JMAP server for just that range, and
  read no further than needed from servers that send the whole file.

### Fixed

- A waiting Ping with a shared folder no longer re-checks the user's own
  folders and folder list at every shared-folder poll, and keeps what it
  caught up on, so later polls do not fetch the same changes again.
- An OAuth token whose JMAP session has no mail account (issued without the
  mail scope) gets 403 instead of a server error.
- The WBXML decoder no longer accepts a global token in place of the root
  element (found by fuzzing).
- Attachment names, and other MIME parameters continued on a new line after
  `=`, are no longer lost (mail-parser 1.0.1).

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
