# Scenarios

End-to-end checks against a running Tolk server, written in a small format
of this project's own and run by the test client in
[`src/bin/eas-client.rs`](../src/bin/eas-client.rs):

```sh
EAS_URL=https://mail.example.com \
EAS_USER1=test1@example.com EAS_PASSWORD_FILE1=~/test1.txt \
EAS_USER2=test2@example.com EAS_PASSWORD_FILE2=~/test2.txt \
./scripts/run-scenarios.sh [scenarios/mail-basics.scn ...]
```

Use two test accounts: the scenarios send mail between them and create
folders and events. Without arguments every `.scn` file runs.

| File | Covers |
|---|---|
| `mail-basics.scn` | Send to itself, push (Ping), mark read, reply, move to Trash. |
| `folders.scn` | Create, rename and delete a folder. |
| `meeting-response.scn` | Account 2 invites account 1, who accepts; the event shows as accepted. |
| `calendar.scn` | An event created on the device is stored and comes back in a fresh sync. |

## In CI

The `integration` job runs them against Stalwart 1.x, Stalwart 0.16 and
Cyrus, each a throwaway container pinned in `scripts/ci/servers/` (Dependabot
keeps those current). `scripts/ci/stalwart.sh` and `scripts/ci/cyrus.sh`
start the servers and create the test users; they work locally too. Cyrus's
test server delivers no mail and serves one request at a time, so it runs
only `folders.scn` and `calendar.scn`.

## Format

One step per line: an `eas-client` command with its arguments, then
checks, each after a `|`. `#` starts a comment and quotes group words.

```text
foldersync 0 | expect Status=1 | folder 2 inbox | folder 4 trash
sync $inbox 0 | expect Status=1 | save SyncKey key
sendmail $user1 "Scenario $run" "Sent by the mail-basics scenario"
ping 60 $inbox | expect Status=2 | retry 90
sync $inbox $key | within "Subject=Scenario $run" | item Subject msg | retry 60
```

Commands are those of `eas-client` (`provision`, `foldersync`, `sync`,
`sendmail`, `ping`, `move`, ...; see the header of `eas-client.rs`), plus
`login <user> <password-file> [device]` to switch accounts.

Checks look at the XML response:

| Check | Meaning |
|---|---|
| `expect Tag=value` | Some element `Tag` has this text; `~` instead of `=` means it contains it. |
| `count Tag=n` | Exactly `n` elements `Tag`. |
| `save Tag name` | Stores the first `Tag` text as `$name`. |
| `folder <type> name` | Stores the ServerId of the first folder of that EAS folder type (2 Inbox, 4 Trash, 8 Calendar, ...). |
| `item Tag name` | Stores the ServerId of the first Add or Change containing `Tag`. |
| `within Tag=value` | Later checks only look at the first Add or Change with that value (`~` works too). |
| `retry seconds` | Repeats the step until its checks pass, for mail or pushes that arrive later. |

Tags match by name or as `Namespace:Name`. In arguments, `$name` is a saved
value; `$run` is unique per run, so subjects never match earlier runs; and
`$user1`, `$password1`, `$user2` and `$password2` come from the environment
variables above.
