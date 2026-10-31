# Tolk

[![CI](https://github.com/RBogie/tolk-jmap/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/RBogie/tolk-jmap/actions/workflows/ci.yml?query=branch%3Amain)
[![Release](https://img.shields.io/github/v/release/RBogie/tolk-jmap?sort=semver)](https://github.com/RBogie/tolk-jmap/releases/latest)
[![Container image](https://img.shields.io/badge/image-ghcr.io%2Frbogie%2Ftolk--jmap-blue?logo=docker&logoColor=white)](https://github.com/RBogie/tolk-jmap/pkgs/container/tolk-jmap)
[![Build provenance](https://img.shields.io/badge/build%20provenance-attested-success?logo=github)](https://github.com/RBogie/tolk-jmap/attestations)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/RBogie/tolk-jmap/badge)](https://scorecard.dev/viewer/?uri=github.com/RBogie/tolk-jmap)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/16127/badge)](https://www.bestpractices.dev/projects/16127)
[![License: AGPL-3.0](https://img.shields.io/github/license/RBogie/tolk-jmap)](LICENSE)

*Tolk* is Dutch for interpreter: it lets phones that speak Exchange
ActiveSync talk to a JMAP mail server.

**[Releases](https://github.com/RBogie/tolk-jmap/releases)** ·
**[Configuration](docs/configuration.md)** ·
**[Compatibility](docs/compatibility.md)** ·
**[Operations](docs/operations.md)** ·
**[Contributing](CONTRIBUTING.md)** ·
**[Security](SECURITY.md)**

An Exchange ActiveSync (EAS) server in front of a JMAP mail server. Phones
add an "Exchange" account and get mail, contacts and calendar with push,
while everything lives on a standards-based JMAP server such as
[Stalwart](https://stalw.art). It does for JMAP what z-push does for IMAP.

```text
 phone (Gmail, iOS Mail, ...)          Tolk                       JMAP server
 ─────────── EAS 14.1/16.1 ──────────▶ WBXML ⇄ JSON ─────────────▶ mail, contacts,
             HTTPS (reverse proxy)     sync state (SQLite)         calendars, push
```

Tolk keeps no mail. It translates each EAS command into JMAP calls
with the device's own credentials, and remembers per device what it has
sent (SyncKeys, item ids), which EAS needs and JMAP does not provide.

## Status

Early, but in daily use: developed against [Stalwart](https://stalw.art)
1.0 with Gmail on Android, and tried against Cyrus. Other
clients (iOS Mail, Samsung Email, Outlook) are implemented for but not yet
tested; reports are welcome, see [docs/testing-a-client.md](docs/testing-a-client.md).

## Features

- Mail: folders, sync windows and filters, read/flag/answered state,
  move and delete, attachments (streamed), search and Find, send, reply
  and forward, drafts with attachments (EAS 16), empty trash,
  out-of-office.
- Contacts (JSContact) and calendars (JSCalendar) both ways, recurrence
  and exceptions, meeting invitations and responses.
- Shared mailboxes, calendars and address books from other accounts.
- Push: Ping and hanging Sync, driven by the JMAP EventSource stream.
- Autodiscover, so a phone needs only an address and a password.
- EAS 14.0, 14.1, 16.0 and 16.1.
- Logins with passwords, app passwords or OAuth tokens from the JMAP server.
- Device management: list, block and wipe devices.
- Operations: health and readiness endpoints, Prometheus metrics, JSON
  logs, daily state backups, login failure limits.

See [docs/compatibility.md](docs/compatibility.md) for what each client
and command does and what is not supported.

## Quick start

Tolk runs as one container behind a TLS reverse proxy (Traefik,
Caddy, nginx). The proxy sends `/Microsoft-Server-ActiveSync` and
`/autodiscover/` to it; everything else stays with the JMAP server.

```sh
docker run -d --name tolk \
  --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges \
  -e JMAP_URL=https://mail.example.com \
  -e TRUSTED_PROXIES=172.16.0.0/12 \
  -e PUBLIC_URL='https://mail.{domain}' \
  -v ./data:/data -e STATE_DB=/data/state.sqlite \
  ghcr.io/rbogie/tolk-jmap:0.1
```

Tolk is a single, fully static binary, and the image contains nothing else
(no distribution, no shell): just the binary and the CA certificates. It
runs as an unprivileged user and needs no capabilities and no writable
filesystem apart from its data volume and `/tmp`, hence the flags above.
Images are published for linux/amd64, arm64, arm/v7, 386, ppc64le and
riscv64; each [release](https://github.com/RBogie/tolk-jmap/releases) also
has the binaries, which run on any Linux without Docker. Pin a version, or
better its digest (in the release notes); there is no `latest` tag.

Releases carry signed build provenance, so you can check that a binary or
image was built by this repository's release workflow from the tagged
source before running it:

```sh
gh attestation verify oci://ghcr.io/rbogie/tolk-jmap:0.1.0 --repo RBogie/tolk-jmap
gh attestation verify tolk-0.1.0-linux-amd64 --repo RBogie/tolk-jmap
```

To build it yourself: `docker build -t tolk .`, or
`docker buildx build --target binary --output dist .` for just the binary.

[deploy/compose.example.yaml](deploy/compose.example.yaml) has a compose
service with Traefik labels. Every setting is described in
[docs/configuration.md](docs/configuration.md), together with what the
JMAP server must offer (and trust the `X-Forwarded-For` header from
Tolk, so its own login limits see the phones' addresses).

On the phone, add an Exchange account with the mail address and password
(an app password works). With Autodiscover routed, nothing else is needed;
otherwise enter the server name, e.g. `mail.example.com`.

## Running it

[docs/operations.md](docs/operations.md) covers upgrades, backups,
devices, traffic capture and troubleshooting.
[docs/releasing.md](docs/releasing.md) describes how releases are made.

## Development

```sh
cargo test                  # unit and integration tests (fake JMAP server)
cargo clippy --all-targets
./scripts/run-scenarios.sh  # end-to-end against a live server, see the script
```

- `src/eas/`: the EAS commands; `src/wbxml/`: the WBXML codec;
  `src/jmap/`: the JMAP client; `src/store/`: device state.
- `tests/support/fake_jmap.rs` is an in-process JMAP server the
  integration tests drive the real router against.
- `src/bin/eas-client.rs` is a scripted EAS client; `scenarios/` holds
  end-to-end scenarios for it ([scenarios/README.md](scenarios/README.md)).
- `fuzz/` has cargo-fuzz targets (nightly).

[docs/ROADMAP.md](docs/ROADMAP.md) lists what is planned.

## License

Tolk is licensed under the [GNU Affero General Public License v3.0](LICENSE)
(AGPL-3.0-only). If you run a modified version for others over a network,
you must offer them its source code.
