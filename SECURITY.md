# Security

## Supported versions

Security fixes go into the latest release. Before 1.0 there are no
maintained older lines: upgrade to the newest version to get a fix.

| Version | Supported |
| --- | --- |
| Latest 0.x release | Yes |
| Older releases | No |

## Reporting a vulnerability

Please report vulnerabilities privately, not in a public issue:
**[report a vulnerability](https://github.com/RBogie/tolk-jmap/security/advisories/new)**
(the repository's Security tab, "Report a vulnerability"). Only the
maintainer sees the report.

Helpful to include: the affected version, how to reproduce it, what an
attacker gains, and any idea for a fix. This is a spare-time project, so
expect a first answer within a week. Once a fix is released, the advisory is
published with credit to the reporter, unless you prefer otherwise.

## Security model

Tolk sees every user's password (EAS uses HTTP Basic auth), or their OAuth
access token, and all their mail in transit. It stores no passwords:
credentials are checked against the JMAP server and only a keyed hash is
kept in memory. The state database holds device ids, folder and item ids and
sync state, not mail. Recommend app passwords to your users.

Deploy it only behind TLS, point `JMAP_URL` at an HTTPS address, and keep
the metrics address and traffic captures private. The image runs as an
unprivileged user and works with a read-only root filesystem and no
capabilities (see [deploy/compose.example.yaml](deploy/compose.example.yaml)).

Releases carry signed build provenance; verify a binary or image before
running it (see the [README](README.md)).
