# Protocol support and clients

Tolk speaks Exchange ActiveSync 14.0, 14.1, 16.0 and 16.1 (configurable with `EAS_VERSIONS`). Devices choose a version when the account is added.

## JMAP servers

| Server | Status |
|---|---|
| Stalwart 1.0 | Tested continuously with Gmail on Android, and in a fresh local install (Docker): sessions with relative URLs, OAuth tokens from its code flow. Earlier versions are not tested. |
| Cyrus IMAP | Tested locally with the Cyrus test image (`cyrus-docker-test-server`): FolderSync, folder create/rename/delete, Sync and creating events work. That image serves one request at a time, so push (EventSource) blocks it, and it delivers no mail, so Ping and invitations could not be tested there. Cyrus offers no JMAP tasks or notes either. |
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
| Provision | Two-phase policy handshake. The policy asks for nothing (no device password, no encryption). Remote wipe is not offered. |
| FolderSync | Mailboxes, address books and calendars, including those other users share with the account (see below). |
| FolderCreate, FolderUpdate, FolderDelete | Mail folders; address books and calendars can be created and renamed, not deleted. A deleted mail folder moves into Trash. Special folders and shared address books and calendars cannot be changed. |
| Sync | Mail (read, flag, delete, move, fetch), contacts and calendar both ways. Hanging (`Wait`, `HeartbeatInterval`), empty and partial Syncs. With 16.x: drafts (save, edit, send, attachments added and removed) and changes to single occurrences by InstanceId. |
| GetItemEstimate | Yes. |
| Ping | Push through the JMAP EventSource stream; folder changes answer status 7. |
| ItemOperations | Fetch (attachments, items, search results) and EmptyFolderContents. Move is not supported. |
| MoveItems | Yes, within mail. |
| SendMail, SmartReply, SmartForward | Yes; retries with the same ClientId are not sent twice. |
| Search | Mailbox (subject and text) and GAL (contacts and correspondents; the Stalwart directory forbids queries). |
| Find (16.1) | Mailbox (free text, one folder or with DeepTraversal its subfolders, or everything) and GAL (at least four characters). Results carry ServerIds that ItemOperations, SmartReply and SmartForward accept. Pictures are not returned. |
| MeetingResponse | Accept, tentative and decline, sent by the JMAP server. |
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
- Contacts: EAS has fixed slots (three emails, two home and two business phones, one mobile, one address per kind, three IM addresses). The device sees the entries that fit; when it edits a card, only the fields it changed are written, into the entries it saw, so other entries stay on the server. A field the device leaves out counts as cleared, unless the device sent a `<Supported>` list without it (MS-ASCMD ghosting). The photo is only changed when the device sends `<Picture>`; an empty one removes it. Photos the server keeps as blobs (as Stalwart does) are downloaded and sent like inline ones. A changed name also rewrites `name/full` from the name components.
- Time zones: devices send Windows `TIME_ZONE_INFORMATION` rules; Tolk picks the IANA zone with the same rules, preferring common names.
