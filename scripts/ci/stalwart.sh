#!/usr/bin/env bash
# Starts a throwaway Stalwart (0.16.x or 1.x) for the integration tests and
# creates two users, user1@example.test and user2@example.test, with password
# $PASSWORD.
#
#   IMAGE=stalwartlabs/stalwart:v1.0.1 PORT=18080 PASSWORD=... scripts/ci/stalwart.sh
#
# Stalwart starts in bootstrap mode: the temporary administrator from
# STALWART_RECOVERY_ADMIN completes the bootstrap, which returns the
# permanent administrator; after a restart, that one creates the users.
# STALWART_PUBLIC_URL makes the JMAP session point at this address.
set -euo pipefail
: "${IMAGE:?}" "${PORT:?}" "${PASSWORD:?}"
NAME=${NAME:-tolk-ci-stalwart}
URL="http://127.0.0.1:$PORT"
USING='"using":["urn:ietf:params:jmap:core","urn:stalwart:jmap"]'

wait_for() {
  for _ in $(seq 1 60); do
    if curl -sf -o /dev/null -L -u "$1" "$URL/.well-known/jmap"; then
      return 0
    fi
    sleep 1
  done
  echo "Stalwart did not come up" >&2
  docker logs "$NAME" >&2
  exit 1
}

# One JMAP request with Stalwart's management capability; prints the response.
api() {
  curl -sSf -u "$1" -H 'Content-Type: application/json' "$URL/jmap/" -d "{$USING,$2}"
}

# Prints a value from the JSON on stdin: `json 'expression on d'`.
json() {
  python3 -c "import json, sys; d = json.load(sys.stdin); print($1)"
}

session_account() {
  curl -sSf -L -u "$1" "$URL/.well-known/jmap" | json 'd["primaryAccounts"]["urn:stalwart:jmap"]'
}

recovery="admin:$(head -c 18 /dev/urandom | base64 | tr -d '/+=')"
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:8080" \
  -e STALWART_RECOVERY_ADMIN="$recovery" \
  -e STALWART_PUBLIC_URL="$URL" \
  "$IMAGE" >/dev/null
wait_for "$recovery"

account=$(session_account "$recovery")
# The hostname property is `serverHostname` in 0.16 and `defaultHostname` in 1.x.
hostname_key=$(api "$recovery" '"methodCalls":[["x:Bootstrap/get",{"accountId":"'"$account"'","ids":null},"g"]]' \
  | json '"defaultHostname" if "defaultHostname" in d["methodResponses"][0][1]["list"][0] else "serverHostname"')
admin=$(api "$recovery" '"methodCalls":[["x:Bootstrap/set",{"accountId":"'"$account"'","update":{"singleton":{"'"$hostname_key"'":"mail.example.test","defaultDomain":"example.test","requestTlsCertificate":false,"generateDkimKeys":false}}},"b"]]' \
  | json 'd["methodResponses"][0][1]["updated"]["singleton"]["username"] + ":" + d["methodResponses"][0][1]["updated"]["singleton"]["secret"]')
docker restart "$NAME" >/dev/null
wait_for "$admin"

account=$(session_account "$admin")
domain=$(api "$admin" '"methodCalls":[["x:Domain/query",{"accountId":"'"$account"'","filter":{"name":"example.test"}},"q"]]' \
  | json 'd["methodResponses"][0][1]["ids"][0]')
user() {
  printf '"%s":{"@type":"User","name":"%s","domainId":"%s","credentials":{"0":{"@type":"Password","secret":"%s"}},"memberGroupIds":{},"aliases":{},"quotas":{},"roles":{"@type":"User"},"permissions":{"@type":"Inherit"},"encryptionAtRest":{"@type":"Disabled"}}' \
    "$1" "$1" "$domain" "$PASSWORD"
}
created=$(api "$admin" '"methodCalls":[["x:Account/set",{"accountId":"'"$account"'","create":{'"$(user user1),$(user user2)"'}},"a"]]' \
  | json 'len(d["methodResponses"][0][1].get("created") or {})')
if [ "$created" != 2 ]; then
  echo "creating the users failed" >&2
  exit 1
fi
for u in user1 user2; do
  wait_for "$u@example.test:$PASSWORD"
done
echo "Stalwart ($IMAGE) ready at $URL"
