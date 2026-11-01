# Roadmap

What is planned or would be welcome. See [compatibility.md](compatibility.md)
for what works today.

## Clients

- Test and record sessions with iOS Mail, Samsung Email and Outlook mobile.
  Only Gmail on Android is tested so far; Autodiscover, hanging Sync and
  partial Syncs are implemented with iOS in mind.
  [testing-a-client.md](testing-a-client.md) lists what to check.
- Gmail keeps showing the user's own meeting response as "invited" (see
  compatibility.md). A Gmail-specific signal would be needed to fix it.

## JMAP servers

- Test against more servers: Cyrus in a full setup (with mail delivery and
  several workers), and Fastmail with a test account.
- Tasks and Notes, for JMAP servers that offer them (neither Stalwart nor
  Cyrus does today).
- Invitations in shared mailboxes: Stalwart does not parse calendar data in
  a shared account.

## Scale and operations

- Large mailboxes: sync state is stored as JSON snapshots per collection.
  A 50,000-message folder syncs incrementally in about 35 ms, so row-based
  tables wait until real mailboxes need them.
