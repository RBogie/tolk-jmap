#!/usr/bin/env bash
# Starts Cyrus IMAP's test server for the integration tests. It comes with
# users user1 to user5 and accepts any password.
#
#   IMAGE=ghcr.io/cyrusimap/cyrus-docker-test-server@sha256:... scripts/ci/cyrus.sh
#
# Its JMAP session always names http://localhost:8080, so it is published on
# exactly that port. The test server handles one request at a time and
# delivers no mail: push and mail delivery cannot be tested against it.
set -euo pipefail
: "${IMAGE:?}"
NAME=${NAME:-tolk-ci-cyrus}

docker run -d --name "$NAME" -p 127.0.0.1:8080:8080 "$IMAGE" >/dev/null
for _ in $(seq 1 90); do
  if curl -sf -o /dev/null -L -u user1:x http://localhost:8080/.well-known/jmap; then
    echo "Cyrus ($IMAGE) ready at http://localhost:8080"
    exit 0
  fi
  sleep 1
done
echo "Cyrus did not come up" >&2
docker logs "$NAME" >&2
exit 1
