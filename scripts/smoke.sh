#!/usr/bin/env bash
# Read-only smoke test of a running service, either shell (genesis-server or
# genesis-cf): the same requests, the same expected answers, so the two can be
# checked against each other and against a deployment.
#
#   scripts/smoke.sh http://127.0.0.1:8080 5042002 [a recorded Safe]
#
# The chain id must be a registry chain of that deployment. Needs curl and jq.
set -uo pipefail

BASE=${1:?base URL}
CHAIN=${2:?a registry chain id}
SAFE=${3:-}
DEAD=0x000000000000000000000000000000000000dEaD

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL  $*"; }
# status and body of a request
req() { curl -s -o /tmp/smoke.$$ -w "%{http_code}" "$@"; }
body() { cat /tmp/smoke.$$; }
expect() { # name, expected status, curl args...
  local name=$1 want=$2; shift 2
  local got; got=$(req "$@")
  [ "$got" == "$want" ] && ok "$name ($got)" || bad "$name: got $got, want $want — $(body | head -c 200)"
}
check() { # name, jq expression that must be true on the last body
  body | jq -e "$2" >/dev/null 2>&1 && ok "$1" || bad "$1 — $(body | head -c 200)"
}

echo "== $BASE"
expect "health" 200 "$BASE/api/health"
check "health is ok" '.ok == true'
check "the registry on chain $CHAIN is v4" "[.registries[] | select(.chainId == $CHAIN)][0].status == \"ok\""
check "health names its shell" '.shell == "docker" or .shell == "cloudflare-workers"'

expect "an unrecorded Safe: 404" 404 "$BASE/api/genesis/$DEAD"
check "  found: false" '.found == false and (.unreachable | length) == 0'
expect "not an address: 400" 400 "$BASE/api/genesis/0x1234"
expect "an unknown route: 404" 404 "$BASE/api/nothing"
expect "a malformed proposal hash: 400" 400 "$BASE/api/proposals/$DEAD/nothash?chainId=$CHAIN"
expect "CORS preflight: 204" 204 -X OPTIONS "$BASE/api/genesis/$DEAD"

expect "a registry page" 200 "$BASE/api/registry/$CHAIN?limit=3"
check "  shape" ".chainId == $CHAIN and .limit == 3 and .order == \"newest\" and (.records | length) <= 3 and (.total | type) == \"number\""
check "  newest first" '[.records[].index] as $i | $i == ($i | sort | reverse)'
expect "oldest first" 200 "$BASE/api/registry/$CHAIN?limit=2&order=oldest"
check "  from index 0" '(.records | length) == 0 or .records[0].index == 0'
expect "limit 0: 400" 400 "$BASE/api/registry/$CHAIN?limit=0"
expect "limit 101: 400" 400 "$BASE/api/registry/$CHAIN?limit=101"
expect "a bad order: 400" 400 "$BASE/api/registry/$CHAIN?order=up"
expect "a chain without the registry: 400" 400 "$BASE/api/registry/999999999"

if [ -z "$SAFE" ]; then
  # the newest record will do
  req "$BASE/api/registry/$CHAIN?limit=1" >/dev/null
  SAFE=$(body | jq -r '.records[0].safe // empty')
fi
if [ -n "$SAFE" ]; then
  expect "a recorded Safe" 200 "$BASE/api/genesis/$SAFE"
  check "  verified, with each registry's name" '.found == true and .verified == true and (.registries | length) >= 1 and (.nameBySafe | type) == "boolean"'
  check "  index and renamedAt are numbers" '(.index | type) == "number" and (.renamedAt | type) == "number"'
  code=$(curl -s -o /dev/null -D - "$BASE/api/genesis/$SAFE" | tr -d '\r' | awk -F': ' 'tolower($1) == "x-cache" {print $2}')
  [ "$code" == "hit" ] && ok "  the second read is a cache hit" || echo "  NOTE  second read x-cache: ${code:-none} (no cache on *.workers.dev / local)"
else
  echo "  NOTE  the registry is empty: no recorded Safe to read"
fi

rm -f /tmp/smoke.$$
echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
