#!/usr/bin/env bash
# Runs every scenario in scenarios/ against a live Tolk with two test
# accounts. Scenarios send mail between those accounts only.
#
#   EAS_URL=https://mail.example.com \
#   EAS_USER1=test1@example.com EAS_PASSWORD_FILE1=~/test1.txt \
#   EAS_USER2=test2@example.com EAS_PASSWORD_FILE2=~/test2.txt \
#   ./scripts/run-scenarios.sh [scenario...]
#
# Scenarios start as account 1 and may switch with `login`.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${EAS_URL:?}" "${EAS_USER1:?}" "${EAS_PASSWORD_FILE1:?}"
cargo build -q --bin eas-client
state=$(mktemp -d)
trap 'rm -rf "$state"' EXIT
failed=0
for file in "${@:-scenarios/*.scn}"; do
  for scenario in $file; do
    echo "== $scenario"
    if ! EAS_CLIENT_STATE=$state ./target/debug/eas-client --url "$EAS_URL" --user "$EAS_USER1" \
        --password-file "$EAS_PASSWORD_FILE1" --device "scenario-$(basename "$scenario" .scn)" \
        scenario "$scenario"; then
      failed=1
    fi
  done
done
exit $failed
