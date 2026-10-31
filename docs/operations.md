# Operator guide

## Upgrades

Tag each image with its git commit as well as `latest`, so you can roll
back (`docker build --build-arg GIT_SHA=$(git rev-parse --short HEAD) -t
tolk:<commit> -t tolk:latest .`). To upgrade, load the new image and
recreate the container. On SIGTERM, waiting Pings are answered at once and other
requests get `SHUTDOWN_DEADLINE_SECS` to finish; give the container a
longer stop timeout than that (`stop_grace_period`).

The state database migrates itself on start. Before each migration a copy
is written next to it (`<STATE_DB>.v<old>-<time>.bak`). A database newer
than the binary is refused, so a rollback needs that copy:

```sh
docker tag tolk:<previous-sha> tolk:latest
# put back the .bak from before the upgrade as state.sqlite, then
docker compose up -d --no-build tolk
```

During a restart phones get a few seconds of errors and retry by
themselves; Gmail may wait a few minutes before its next push request.

## Backups

Only sync bookkeeping is stored; mail, contacts and calendars are on the
JMAP server. A copy of the database is made daily (`BACKUP_DIR`,
`BACKUP_KEEP`). Restoring an older copy is safe: devices that are ahead of
it resync the affected folders (see configuration.md).

The image has no shell or tools, only the `tolk` binary. Work on the data
volume from the host, or with a throwaway container that mounts it, e.g.
`docker run --rm -it -v ./tolk/data:/data debian:trixie-slim`.

## Devices

```sh
docker exec tolk tolk devices list
docker exec tolk tolk devices block <user> <device>
docker exec tolk tolk devices wipe <user> <device>          # account only, EAS 16.1
docker exec tolk tolk devices wipe <user> <device> --full   # erases the phone
docker exec tolk tolk devices forget <user> <device>
docker exec tolk tolk devices show <user> <device>
docker exec tolk tolk devices resync <user> <device> [<collection>]
docker exec tolk tolk devices clear-quarantine <user> <device>
```

A lost phone: `block` stops it at once; `wipe` removes the account (or
the whole phone with `--full`) the next time it connects. Changing the
account's password also stops it.

`show` prints what the server holds for a device: its FolderSync key,
each collection (EAS collection id, the JMAP folder behind it, SyncKey,
how many items the device has, when its state last changed and the sticky
options: FilterType, body types, number of `<Supported>` contact fields),
the last Ping parameters, quarantined items and the last command that
failed with an error.

A device that is stuck: `resync` it. With a collection id (from `show`)
only that folder starts over: the device's next Sync of it gets "invalid
SyncKey" (status 3) and the device downloads it again. Without one, the
folder hierarchy and every collection start over: FolderSync gets status 9
and the device resyncs everything by itself, keeping the account. As a last
resort `forget` it; it starts over on its next request (it may need the
account removed and added again).

A device that kept failing on an item gets it quarantined (log: "device
keeps failing on this item", and `show` lists it). `clear-quarantine`
sends those items again at its next Sync, for example after an upgrade
that fixes the item; if the device still fails on one, it is quarantined
again.

## Traffic capture

For debugging a client, capture its EAS traffic: set `TRAFFIC_DIR` and
list the user (or `user/device`) in `TRAFFIC_CAPTURE`. The files contain
complete messages; they are written with mode 0600 and deleted after
`TRAFFIC_RETENTION_HOURS` or `TRAFFIC_RETENTION_DAYS` (one day by
default). Capture only test accounts or for a short time, and never share
captures: turn them into fixtures with `eas-sanitize` and review the
`.xml` output first.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Account setup opens a Microsoft sign-in page, then "can't reach server" | Gmail (and Outlook) ask Microsoft's cloud Autodiscover first. If the address is also the sign-in name of a Microsoft (Outlook.com) account, Microsoft claims it and the app tries Microsoft's login; your server is never asked. Check with `curl "https://outlook.office365.com/autodiscover/autodiscover.json?Email=<address>&Protocol=ActiveSync"`: a `eas.outlook.com` URL means Microsoft claims it. Use the app's manual setup with the server name, or remove that address from the Microsoft account. |
| Phone keeps asking for the password | The JMAP server rejects the login (wrong or changed password, an app password needed), or the address hit the failed-login limit (log: "too many failed logins"); it clears after 15 minutes. |
| Requests answered with status 142 in a loop | The device lost its policy key; it provisions again by itself. A storm of them means a client bug; the log says "ping loop" and answers are slowed down. |
| Sync status 3 (invalid SyncKey) | The device and server disagree about what was sent, e.g. after a database restore; the device resyncs that folder. Repeated for one device: `devices show` it, then `devices resync` (or, as a last resort, `forget`) it. |
| Log: "sync loop: sending one item at a time", then "device keeps failing on this item" | The device kept rejecting a Sync response (it sent the same SyncKey three times; slow responses that time out on the device look the same). Tolk sent the items of that response one at a time, then normal batches again; an item the device also rejected three times is skipped for that device from now on, and the log names its JMAP id. The item stays on the server and on other devices. |
| Sync status 12 or Ping status 7 | The folder list changed (a folder was added, renamed or deleted elsewhere); the device runs FolderSync and carries on. |
| No push, only manual refresh | The phone's own setting (Gmail: "Tap to be notified" on the account); or the JMAP push stream is down (log: "push stream failed"), in which case folders are still checked every two minutes. |
| New mail arrives late after an upgrade | The client backs off after the restart's errors; it recovers within minutes. |
| HTTP 403 | The device is blocked or was wiped (`devices list`). |
| HTTP 503 | The JMAP server or the database is unavailable (`/readyz` says which; the log has the details) or a concurrency limit was hit (`MAX_PINGS`, `MAX_COMMANDS`). |

Logs carry the request id, user, device and command on every line
(`LOG_FORMAT=json` for log collectors). `RUST_LOG=info` keeps message
details out of the log.
