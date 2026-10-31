# Security

Tolk sees every user's password (EAS uses HTTP Basic auth), or their
OAuth access token, and all their mail in transit. It stores no
passwords: credentials are checked against the JMAP server and only a
keyed hash is kept in memory. The state
database holds device ids, folder and item ids and sync state, not mail.

Deploy it only behind TLS, point `JMAP_URL` at an HTTPS address, and keep
the metrics address and traffic captures private.

To report a vulnerability, use GitHub's private vulnerability reporting
(the repository's Security tab) rather than a public issue, with a
description and steps to reproduce.
