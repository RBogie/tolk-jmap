# Changelog

## Unreleased

### Added

- Windows: a native binary (`tolk-<version>-windows-amd64.exe`) in each
  release, with signed provenance, and Windows in CI. It stops on Ctrl-C,
  Ctrl-Break, closing the console, logoff and shutdown, and trusts the
  Windows certificate store. operations.md says how to protect its data
  folder, since Windows does not get the Unix file modes.

### Changed

- Dates and time zones are handled with jiff instead of chrono and
  chrono-tz, which RustSec lists as unmaintained (RUSTSEC-2026-0543 and
  RUSTSEC-2026-0555). The binary carries jiff's copy of the time zone
  database (2026f), so it still needs no zoneinfo files.

### Fixed

- Moving a series to another time on a device moves its occurrences with
  it: an occurrence's key is its original start, so before, a moved or
  changed occurrence was left under its old key, naming an occurrence that
  no longer exists, and a new one without its own fields was added (#24).
- A time change sent with the zone's rules but no names (as iOS sends it)
  keeps the event's own zone name, for example Europe/Zurich, instead of
  the most common zone with the same rules.
- Calendar recurrence:
  - The nth day, weekday or weekend day of a month or year (for example
    "last weekday") is stored as such (`bySetPosition`) instead of as an
    occurrence on every day of the week, and reaches the device as such
    instead of being left out.
  - Patterns EAS cannot express are left out instead of sent as another
    pattern: every Monday of a month (sent as a day of the month), a fifth
    or second-to-last weekday (sent as the last), different weekdays with
    different weeks, or several days of the month.
- One occurrence of a series:
  - Its description, reminder, busy status, categories and, for a meeting
    the user organizes, attendees are stored when the device changes them
    (with an InstanceId or in `<Exceptions>`), and sent to the device.
    Before, only the title, location and times were.
  - A field equal to the series is not stored for the occurrence, so a later
    change to the series reaches it.
  - A Sensitivity of its own has no place in JSCalendar: the change is
    refused (status 6), or with `UNSUPPORTED_FIELDS=log` stored without it.
  - Occurrences the JMAP server stores as paths into the series' values
    (as Stalwart does) are shown with those values, and an edit replaces
    such a path instead of writing both.
  - An occurrence sent back with its description truncated, as the device
    was shown it, keeps the whole description.
- Meetings the user organizes under an alias: the user is no longer added
  as an attendee of their own meeting, and their existing entry is kept.
- A large folder paged through while a message in it is deleted or moved
  no longer leaves another message out (the first sync would miss it, a
  resync could delete it from the device): paging starts over when the
  JMAP server's `queryState` changes (RFC 8620 5.5).
- Calls that would exceed the JMAP server's `maxObjectsInGet` or
  `maxObjectsInSet` (a Sync window above 500, or more than 500 changes in
  one Sync) are split instead of failing on every retry.
- Request bodies are read within `MAX_COMMANDS` (busy: 503 before the
  upload is taken in) and must arrive within 5 minutes (else 408).
- A request that arrives while the device's hanging Sync or Ping is still
  starting ends it, instead of waiting for it for up to an hour.
- The push stream's planned renewal every 30 minutes is no longer logged as
  "push stream failed", and changes made while the stream reconnects are
  caught up.
- ItemOperations answers at most 50 Fetches per request; the rest get
  status 3.
- Time zones that change for good during a year, as Alberta, British
  Columbia, Manitoba and Cuba do in 2026 by keeping daylight time, reach the
  device with the offset they have from then on. EAS describes a zone as
  one offset or a yearly switch to daylight time and back, which cannot say
  "once".
- MeetingResponse:
  - With EAS 16, the organizer now gets a reply when the device asks for
    one (`SendResponse`): the JMAP server sends it. Before, no reply was
    sent.
  - Answering one occurrence of a series (InstanceId) only answers that
    occurrence instead of the whole series.
- Contacts:
  - Car, radio, assistant, company main and MMS phones, the assistant,
    manager and children, the Yomi (phonetic) names and the office location
    are kept instead of dropped. CustomerId, GovernmentId and AccountName
    have no JSContact property: an Add or Change with one of them is now
    refused (status 6) instead of stored without it, unless
    `UNSUPPORTED_FIELDS=log` (store the rest and log the field names).
  - Deleting a card that is in several address books only takes it out of
    that folder's book.
  - When the device moves an email or IM address to another slot, it keeps
    its own entry (contexts, label).
  - A name edit keeps how the full name is written where it can, and
    otherwise follows the card's order and separators. An edited address
    field keeps its place among the components.
- A device reusing a ClientId in a later Sync gets a new object instead of
  the one an earlier Add created (MS-ASCMD 2.2.3.28.2: a ClientId is only
  unique within one Sync).
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
