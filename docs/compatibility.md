# Protocol support and clients

Tolk speaks Exchange ActiveSync 14.0, 14.1, 16.0 and 16.1 (configurable with `EAS_VERSIONS`). Devices choose a version when the account is added.

## JMAP servers

| Server | Status |
|---|---|
| Stalwart 1.0 | Tested continuously with Gmail on Android, and in CI with every scenario (mail, push, folders, calendar, meeting responses). OAuth tokens from its code flow were tested by hand. |
| Stalwart 0.16 | Tested in CI with every scenario, on the last 0.16 release. |
| Cyrus IMAP | Tested in CI with the Cyrus test image (`cyrus-docker-test-server`): FolderSync, folder create/rename/delete, Sync and creating events. That image serves one request at a time, so push (EventSource) blocks it, and it delivers no mail, so Ping and invitations are not tested there. Cyrus offers no JMAP tasks or notes either. |
| Fastmail | Not tested: there is no self-hosted Fastmail, and a test account is needed. |

## Clients

| Client | Status |
|---|---|
| Gmail on Android | Tested continuously (emulator), with 14.1 and 16.1: mail, push, contacts, calendar (16.1: locations, moving and deleting single occurrences), invitations, search, folders, Autodiscover. Its own meeting response stays "invited": Gmail treats the user's own response as device data and ignores the server's `ResponseType` and `AttendeeStatus` for it, even in an attendees-only Change. The server, the JMAP account and the organizer all have the right status. Gmail keeps drafts on the phone and does not sync the Drafts folder. |
| iOS Mail | Not tested. Autodiscover, hanging Sync and empty or partial Syncs are implemented for it. |
| Samsung Email | Not tested. |
| Outlook mobile | Not tested; it reaches servers through Microsoft's cloud, which needs public reachability and Autodiscover. |
| Outlook for Windows, classic (2013 to 2021, Microsoft 365) | Can use EAS with non-Microsoft servers through a manually created profile (Control Panel, Mail, add an account by hand, "Exchange ActiveSync", server name entered by hand). Not tested with Tolk. |
| New Outlook for Windows | Dropped third-party EAS accounts; use IMAP against Stalwart instead. |
| macOS Mail, Thunderbird | Not EAS clients (Thunderbird only through add-ons such as TbSync, not tested); use IMAP, CalDAV and CardDAV against Stalwart instead. |

## Commands

| Command | Support |
|---|---|
| Provision | Two-phase policy handshake. The policy asks for nothing (no device password, no encryption). A remote wipe is sent when requested with `tolk devices wipe`. |
| FolderSync | Mailboxes, address books and calendars, including those other users share with the account (see below). |
| FolderCreate, FolderUpdate, FolderDelete | Mail folders; address books and calendars can be created and renamed, not deleted. A deleted mail folder moves into Trash. Special folders and shared address books and calendars cannot be changed. |
| Sync | Mail (read, flag, delete, move, fetch), contacts and calendar both ways. Hanging (`Wait`, `HeartbeatInterval`), empty and partial Syncs. With 16.x: drafts (save, edit, send, attachments added and removed) and changes to single occurrences by InstanceId. |
| GetItemEstimate | Yes. |
| Ping | Push through the JMAP EventSource stream; folder changes answer status 7. |
| ItemOperations | Fetch (attachments, items, search results) and EmptyFolderContents. Move is not supported. |
| MoveItems | Yes, within mail. |
| SendMail, SmartReply, SmartForward | Yes; retries with the same ClientId are not sent twice, also when the JMAP server's answer to the first attempt was lost (the retry checks whether it went out). A message the server refuses leaves no copy in Sent. |
| Search | Mailbox (subject and text) and GAL (contacts and correspondents; the Stalwart directory forbids queries). |
| Find (16.1) | Mailbox (free text, one folder or with DeepTraversal its subfolders, or everything) and GAL (at least four characters). Results carry ServerIds that ItemOperations, SmartReply and SmartForward accept. Pictures are not returned. |
| MeetingResponse | Accept, tentative and decline, for a whole series or (with an InstanceId) one occurrence. The reply to the organizer: up to 14.1 the device sends it itself; with 16.x the JMAP server sends it when the device asks (`SendResponse`), with the server's own text: a body or a proposed new time in the request is not passed on. |
| Settings | Device information, user information and out-of-office (the JMAP vacation response: one message for all audiences). |
| ResolveRecipients | Contacts and correspondents; no certificates, no free/busy. |
| Autodiscover | Mobilesync XML and the v2 JSON form (see configuration.md for routing). |

Not supported: ValidateCert, GetAttachment (pre-14 form), Tasks and Notes, SMS, S/MIME, IRM.

### Shared mailboxes, calendars and address books

Every account in the user's JMAP session that offers calendars or contacts is synced, not only the user's own: calendars and address books other users share (in Stalwart through their sharing settings, `shareWith`) appear as extra folders named "<name> (<owner>)", with the owner's account name from the session. They are never the device's main calendar or contacts folder. Sync, device changes, Ping and meeting responses work in the folder's own account. Stalwart's push stream only reports the user's own account, so changes in shared folders reach the device at the next periodic check of a waiting Ping or Sync (every two minutes by default, `SHARED_POLL_SECS`) or its next Sync, not at once. Changed sharing rights take effect when the cached JMAP session expires (`AUTH_CACHE_TTL_SECS`).

A shared folder the user may only read (the account is `isReadOnly`, or the calendar lacks `mayWriteAll`, or the address book lacks `mayWrite`) refuses changes from the device: a Change or Delete gets Sync status 7 (conflict, the server wins) and the device is sent the server's version again; an Add gets status 6.

Mailboxes other users share appear the same way: the top shared mailboxes are named "<name> (<owner>)" with their subfolders below them, all as plain mail folders (never the device's Inbox, Sent or Drafts). Sync (read, flag, delete into the owner's Trash), fetching, attachments, Search and Find in one folder, SmartReply and SmartForward work in the owner's account; replies are always sent from the user's own account. MoveItems between a shared mailbox and the user's own copies the message over and removes the original (`Email/copy`). Drafts cannot be kept in a shared mailbox, and shared mailboxes cannot be renamed, deleted or get subfolders from the phone. Like shared calendars, changes arrive with the `SHARED_POLL_SECS` check.

### SMS

SMS sync is an EAS 14.0 feature for phones that store text messages on the server. Tolk offers no SMS folder in FolderSync and does not advertise SMS in Settings, so clients never try to sync or send SMS through it, and nothing is retried.

## Data mapping notes

- Mail versions cover read, flagged, answered and forwarded; other keywords do not reach the device.
- `Importance` comes from the `Importance` or `X-Priority` header; conversations from the JMAP thread.
- Calendar events use JSCalendar as Stalwart implements it (2.0 draft): see configuration.md.
- Conflicts (MS-ASCMD 2.2.3.34): a Sync Change for an item the server changed since the device last received it (a contact, an event, or a message's read and flag state) is discarded with status 7, and the server's version is sent in the same response, unless the device asked for `<Conflict>0</Conflict>` (the device wins). A Delete always wins.
- Device changes follow MS-ASCMD 2.2.3.24 on fields the device leaves out. Calendar: a left-out description or `<Exceptions>` stays as it is, as does a single occurrence left out of `<Exceptions>`; with 16.x every left-out field stays, with 14.x a left-out field is cleared unless the device's `<Supported>` list leaves it out. Occurrences keep what the device leaves out of them. Drafts (16.x): a left-out To, Cc, Bcc or Subject is removed, a left-out body stays, and a reply keeps its thread. A draft body may be plain text, HTML or a whole MIME message (as iOS Mail sends it), whose text, HTML, files and headers make the draft (the fields the device sends win, From is the account's); an RTF body is refused (or with `UNSUPPORTED_FIELDS=log` left out).
- Contacts: EAS has fixed slots (three emails, two home and two business phones, one mobile, one address per kind, three IM addresses). Car, radio, assistant and MMS phones are phones with that `label`, the company main number one with the `main-number` feature; the assistant, manager, spouse and children are `relatedTo` entries (relations `agent`, `manager`, `spouse`, `child`; `manager` is not an RFC 9553 type); the Yomi fields are the `phonetic` of the given and surname components and the organization's `sortAs`; the office is the `room` of the work address. CustomerId, GovernmentId and AccountName have no place in JSContact and are not kept (an Add or Change with one is refused, or with `UNSUPPORTED_FIELDS=log` stored without it). A Delete takes a card that is in several address books out of that folder's book only. Emails and IM addresses keep their entries when the device moves them to another slot. The device sees the entries that fit; when it edits a card, only the fields it changed are written, into the entries it saw, so other entries stay on the server. A field the device leaves out counts as cleared, unless the device sent a `<Supported>` list without it (MS-ASCMD ghosting). The photo and the notes are only changed when the device sends `<Picture>` or `<Body>`; an empty one removes them. Notes are plain text: an HTML or MIME body is stored as its text; an RTF one, or a MIME one with attachments, is refused (or with `UNSUPPORTED_FIELDS=log` left out, or without the attachments). Photos the server keeps as blobs (as Stalwart does) are downloaded and sent like inline ones. A changed name also rewrites `name/full` from the name components.
- Time zones: devices send Windows `TIME_ZONE_INFORMATION` rules; Tolk picks the IANA zone with the same rules, preferring common names. When a device edits an event and sends a zone with the same rules as the event's own (iOS drops the names), the event keeps its own zone, also when the time changes.
- Single occurrences (MS-ASCAL 2.2.2.21) carry their own title, location, times, description, reminder, busy status, categories and (for a meeting the user organizes) attendees, both ways, with `<Exceptions>` and with a 16.x change by InstanceId. A field equal to the series is not stored for the occurrence, so a later change to the series reaches it. JSCalendar allows no `privacy` in an occurrence (RFC 8984 4.3.5): a change that gives one occurrence its own Sensitivity is refused, or with `UNSUPPORTED_FIELDS=log` stored without it.
- Descriptions are plain text or HTML (`descriptionContentType`), for the event and for one occurrence. The device gets HTML when it asks for HTML, otherwise text; an HTML body from the device is stored as HTML, and a MIME one as its HTML part or else its text. An RTF body, or a MIME one with attachments, is refused (or with `UNSUPPORTED_FIELDS=log` left out, or without the attachments).
- Recurrence: the nth day, weekday or weekend day of the month (DayOfWeek 127, 62 or 65 with a WeekOfMonth) is the day list with `bySetPosition`; one weekday is an `nthOfPeriod`. Rules EAS cannot say (every Monday of a month, a fifth or second-to-last weekday, several month days, positions in a week) are not shown as a recurrence, so the device shows the first occurrence only. `Until` is the start of the last occurrence (MS-ASCAL 2.2.2.47).
- Calendar edits only write what the device changed: a field it sends back as it was shown stays as it is on the server, so what EAS can show only partly (several reminders or locations, a `secret` event, a recurrence rule EAS cannot express) survives an edit on the phone.
