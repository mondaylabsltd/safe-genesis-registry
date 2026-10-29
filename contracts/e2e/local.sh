#!/usr/bin/env bash
# End-to-end regression check of SafeGenesisRegistry on a local chain.
#
# Deploys the registry the way production does (deterministic deployer, the
# salt in script/Deploy.s.sol) on a fresh anvil, sends real transactions and
# reads the state back: events, raw storage layout, write-once behaviour,
# every revert with its arguments, name edge cases, pagination against
# safeAt, the eth_call cost of pages, a replay from deployCallOf, and renames
# (only by the Safe; on the fork, a real Safe through execTransaction).
#
#   contracts/e2e/local.sh                                      # local chain only
#   FORK_RPC_GNOSIS=https://rpc.gnosischain.com contracts/e2e/local.sh   # + real Safe v1.4.1 on a fork
#
# Needs anvil, forge, cast, jq and python3. Exits non-zero on any failure.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT=${PORT:-8547}
RPC=http://127.0.0.1:$PORT
ANVIL_PIDS=()
# Local and fork runs must not land in broadcast/, which records real deployments
# (a Gnosis fork has chain id 100, like Gnosis itself).
export FOUNDRY_BROADCAST; FOUNDRY_BROADCAST=$(mktemp -d)
cleanup() { for p in "${ANVIL_PIDS[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$FOUNDRY_BROADCAST"; }
trap cleanup EXIT

start_anvil() { # port [fork-url]
  if [ -n "${2:-}" ]; then anvil --port "$1" --fork-url "$2" --silent & else anvil --port "$1" --silent & fi
  ANVIL_PIDS+=($!)
  for _ in $(seq 1 60); do cast chain-id --rpc-url "http://127.0.0.1:$1" >/dev/null 2>&1 && return 0; sleep 0.5; done
  echo "anvil on port $1 did not start" >&2; exit 2
}

# anvil's default accounts 0..2
K0=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
K1=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
K2=0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a
A0=$(cast wallet address $K0); A1=$(cast wallet address $K1); A2=$(cast wallet address $K2)
Z=0x0000000000000000000000000000000000000000

deploy_registry() { # rpc -> prints the address; checks it is the predicted one
  local out; out=$(forge script script/Deploy.s.sol --rpc-url "$1" --broadcast --private-key $K0 2>&1)
  echo "$out" | grep -q "deployed on chain\|already deployed" || { echo "$out" >&2; exit 2; }
  echo "$out" | grep "SafeGenesisRegistry address:" | awk '{print $3}'
}

echo "== 0. deploy (deterministic deployer, production salt)"
forge build -q
start_anvil $PORT
R=$(deploy_registry $RPC)
echo "  registry at $R"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL  $*"; }
eq()  { if [ "$(echo "$2" | tr A-F a-f)" == "$(echo "$3" | tr A-F a-f)" ]; then ok "$1"; else bad "$1: got [$2] want [$3]"; fi; }

eq "runtime code == this build" "$(cast code $R --rpc-url $RPC)" "$(forge inspect SafeGenesisRegistry deployedBytecode)"
eq "VERSION" "$(cast call $R 'VERSION()(uint8)' --rpc-url $RPC)" "$(grep -oE 'VERSION = [0-9]+' src/SafeGenesisRegistry.sol | grep -oE '[0-9]+')"
eq "starts empty" "$(cast call $R 'totalRecords()(uint256)' --rpc-url $RPC)" 0
F=$(forge create test/fixtures/SafeLike.sol:SafeLikeProxyFactory --rpc-url $RPC --private-key $K0 --broadcast 2>&1 | grep "Deployed to" | awk '{print $3}')
S=$(forge create test/fixtures/SafeLike.sol:MockSafeSingleton --rpc-url $RPC --private-key $K0 --broadcast 2>&1 | grep "Deployed to" | awk '{print $3}')

GT='(address,address,uint64,uint256,bytes,bytes32,address,uint64,uint32,string)'
SIG_RECORD='record(address,bytes,address,bytes,uint256,string)'
CODE=$(cast call $F 'proxyCreationCode()(bytes)' --rpc-url $RPC)
init() { cast calldata 'setup(address[],uint256,address,bytes,address,address,uint256,address)' "[$1]" "$2" 0x0000000000000000000000000000000000000000 0x 0x0000000000000000000000000000000000000000 0x0000000000000000000000000000000000000000 0 0x0000000000000000000000000000000000000000; }
INIT=$(init "$A0,$A1,$A2" 2)

# simulate, then send; echoes "safe created"
simulate() { cast call $R "$SIG_RECORD(address,bool)" $F $CODE $S "$1" "$2" "$3" --from "${4:-$A0}" --rpc-url $RPC | tr '\n' ' '; }
send() { cast send $R "$SIG_RECORD" $F $CODE $S "$1" "$2" "$3" --private-key "$4" --rpc-url $RPC --json; }
# full revert data (selector + args) of a would-be write
revert_of() {
  local cd; cd=$(cast calldata "$SIG_RECORD" "$@")
  cast rpc eth_call "{\"to\":\"$R\",\"data\":\"$cd\"}" latest --rpc-url $RPC 2>&1 | grep -oE '0x[0-9a-f]{8}([0-9a-f]{64})*"?' | tail -1 | tr -d '"'
}
err() { cast calldata "$@"; }
view_revert_from() { cast rpc eth_call "{\"from\":\"$1\",\"to\":\"$R\",\"data\":\"$2\"}" latest --rpc-url $RPC 2>&1 | grep -oE '0x[0-9a-f]{8}([0-9a-f]{64})*"?' | tail -1 | tr -d '"'; }
# A Safe's own call: execTransaction by an owner of a 1-threshold Safe, with
# the owner's pre-validated signature (r = owner, s = 0, v = 1).
safe_exec() { # rpc safe to data key
  local owner sig; owner=$(cast wallet address "$5")
  sig=0x$(cast abi-encode 'f(address,uint256)' $owner 0 | cut -c3-)01
  cast send "$2" 'execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes)' \
    "$3" 0 "$4" 0 0 0 0 $Z $Z $sig --private-key "$5" --rpc-url "$1" --json
}
view_revert() { cast rpc eth_call "{\"to\":\"$R\",\"data\":\"$1\"}" latest --rpc-url $RPC 2>&1 | grep -oE '0x[0-9a-f]{8}([0-9a-f]{64})*"?' | tail -1 | tr -d '"'; }

echo "== 1. first write, named, from account 1"
read SAFE CREATED <<<"$(simulate "$INIT" 7 "Team fund" $A1)"
eq "simulated created" "$CREATED" true
EXPECTED=$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $F $CODE $S "$INIT" 7 --rpc-url $RPC)
eq "computeAddress agrees" "$SAFE" "$EXPECTED"
RCPT=$(send "$INIT" 7 "Team fund" $K1)
eq "tx status" "$(echo "$RCPT" | jq -r .status)" 0x1
BLOCK=$(cast to-dec "$(echo "$RCPT" | jq -r .blockNumber)")
echo "  gas used (first write): $(cast to-dec "$(echo "$RCPT" | jq -r .gasUsed)")"
eq "one log" "$(echo "$RCPT" | jq '.logs | length')" 1
TOPICS=$(echo "$RCPT" | jq -r '.logs[0].topics | join(" ")')
read T0 T1 T2 T3 <<<"$TOPICS"
eq "topic0 = GenesisRecorded(...,string)" "$T0" "$(cast keccak 'GenesisRecorded(address,address,address,uint256,bytes32,bytes,string)')"
eq "topic1 = safe" "$T1" "$(cast abi-encode "f(address)" $SAFE)"
eq "topic2 = factory" "$T2" "$(cast abi-encode 'f(address)' $F)"
eq "topic3 = singleton" "$T3" "$(cast abi-encode 'f(address)' $S)"
DATA=$(cast abi-decode --input 'f(uint256,bytes32,bytes,string)' "$(echo "$RCPT" | jq -r '.logs[0].data')" | tr '\n' '|')
IFS='|' read EV_NONCE EV_HASH EV_INIT EV_NAME <<<"$DATA"
eq "event saltNonce" "$EV_NONCE" 7
eq "event codeHash" "$EV_HASH" "$(cast keccak $CODE)"
eq "event initializer" "$EV_INIT" "$INIT"
eq "event name" "$EV_NAME" '"Team fund"'

echo "== 2. read back: genesisOf, isRecorded, safeAt, totalRecords"
eq "found" "$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC | head -1)" true
eq "isRecorded" "$(cast call $R 'isRecorded(address)(bool)' $SAFE --rpc-url $RPC)" true
eq "totalRecords" "$(cast call $R 'totalRecords()(uint256)' --rpc-url $RPC)" 1
eq "safeAt(0)" "$(cast call $R 'safeAt(uint256)(address)' 0 --rpc-url $RPC)" "$SAFE"
# fields through a typed decode
FIELDS=$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC --json | jq -r '.[1]')
for want in "$F" "$S" "$A1" "$BLOCK" '"Team fund"'; do
  echo "$FIELDS" | tr A-F a-f | grep -q "$(echo "$want" | tr A-F a-f)" && ok "record holds $want" || bad "record lacks $want"
done

echo "== 3. raw storage: slot packing and the array"
BASE=$(cast index address $SAFE 0)
slotp() { cast storage $R "$(python3 -c "print(hex(int('$1',16)+$2))")" --rpc-url $RPC; }
eq "slot+0 factory" "$(slotp $BASE 0)" "$(cast abi-encode 'f(address)' $F)"
P1=$(slotp $BASE 1)
eq "slot+1 singleton (low 20 bytes)" "0x${P1:26:40}" "$S"
eq "slot+1 renamedAt (next 8) = 0" "$(cast to-dec 0x${P1:10:16})" 0
eq "slot+2 saltNonce" "$(cast to-dec $(slotp $BASE 2))" 7
P=$(slotp $BASE 5)
echo "  slot+5 = $P   (index | recordedAt | recorder, high to low)"
eq "slot+5 recorder (low 20 bytes)" "0x${P:26:40}" "$A1"
eq "slot+5 recordedAt (next 8)" "$(cast to-dec 0x${P:10:16})" "$BLOCK"
eq "slot+5 index (next 4)" "$(cast to-dec 0x${P:2:8})" 0
NAMESLOT=$(slotp $BASE 6)
echo "  slot+6 = $NAMESLOT   (short string: bytes, then 2*len)"
eq "name length byte (2*9)" "$(cast to-dec 0x${NAMESLOT:64:2})" 18
eq "slot+7 is empty (struct has 7 slots)" "$(slotp $BASE 7)" 0x0000000000000000000000000000000000000000000000000000000000000000
eq "array length (slot 1)" "$(cast to-dec $(cast storage $R 1 --rpc-url $RPC))" 1
ARR=$(cast keccak 0x0000000000000000000000000000000000000000000000000000000000000001)
eq "array[0]" "$(slotp $ARR 0)" "$(cast abi-encode 'f(address)' $SAFE)"

echo "== 4. a repeat write: no change, no event, first name kept"
read S2 C2 <<<"$(simulate "$INIT" 7 "Something else" $A2)"
eq "repeat: same safe" "$S2" "$SAFE"; eq "repeat: created=false" "$C2" false
RCPT2=$(send "$INIT" 7 "Something else" $K2)
eq "repeat: tx ok" "$(echo "$RCPT2" | jq -r .status)" 0x1
eq "repeat: no log" "$(echo "$RCPT2" | jq '.logs | length')" 0
echo "  gas used (repeat): $(cast to-dec "$(echo "$RCPT2" | jq -r .gasUsed)")"
eq "repeat: total unchanged" "$(cast call $R 'totalRecords()(uint256)' --rpc-url $RPC)" 1
echo "$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC)" | grep -q '"Team fund"' && ok "repeat: name kept" || bad "repeat: name changed"
echo "$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC)" | tr A-F a-f | grep -q "$(echo $A1 | tr A-F a-f)" && ok "repeat: recorder kept" || bad "repeat: recorder changed"

echo "== 5. refusals (each one a would-be write; the state must not move)"
LONG=$(python3 -c "print('x'*65)")
BIG=0x$(python3 -c "print('ab'*8193)")
eq "empty name -> EmptyName"           "$(revert_of $F $CODE $S "$INIT" 8 "")"        "$(err 'EmptyName()')"
eq "65-byte name -> NameTooLong(65)"    "$(revert_of $F $CODE $S "$INIT" 8 "$LONG")"   "$(err 'NameTooLong(uint256)' 65)"
eq "zero factory -> ZeroAddress"        "$(revert_of $Z $CODE $S "$INIT" 8 n)"         "$(err 'ZeroAddress()')"
eq "zero singleton -> ZeroAddress"      "$(revert_of $F $CODE $Z "$INIT" 8 n)"         "$(err 'ZeroAddress()')"
eq "empty code -> EmptyProxyCreationCode" "$(revert_of $F 0x $S "$INIT" 8 n)"          "$(err 'EmptyProxyCreationCode()')"
eq "empty init -> EmptyInitializer"     "$(revert_of $F $CODE $S 0x 8 n)"              "$(err 'EmptyInitializer()')"
eq "8193-byte init -> InitializerTooLong" "$(revert_of $F $CODE $S $BIG 8 n)"          "$(err 'InitializerTooLong(uint256)' 8193)"
eq "bad name on a recorded Safe still reverts" "$(revert_of $F $CODE $S "$INIT" 7 "")" "$(err 'EmptyName()')"
eq "safeAt(1) -> IndexOutOfBounds(1,1)" "$(view_revert "$(cast calldata 'safeAt(uint256)' 1)")" "$(err 'IndexOutOfBounds(uint256,uint256)' 1 1)"
# a reverted transaction actually mined: state unchanged
FAILED=$(cast send $R "$SIG_RECORD" $F $CODE $S "$INIT" 9 "" --private-key $K0 --rpc-url $RPC --gas-limit 500000 --json 2>/dev/null)
eq "mined revert: status 0" "$(echo "$FAILED" | jq -r .status)" 0x0
eq "mined revert: total unchanged" "$(cast call $R 'totalRecords()(uint256)' --rpc-url $RPC)" 1
eq "mined revert: nothing at that Safe" "$(cast call $R 'isRecorded(address)(bool)' $(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $F $CODE $S "$INIT" 9 --rpc-url $RPC) --rpc-url $RPC)" false

echo "== 6. edge names: exactly 64 bytes, multibyte UTF-8, raw non-UTF-8 bytes"
N64=$(python3 -c "print('y'*64)")
send "$INIT" 100 "$N64" $K0 >/dev/null
SAFE100=$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $F $CODE $S "$INIT" 100 --rpc-url $RPC)
cast call $R "genesisOf(address)(bool,$GT)" $SAFE100 --rpc-url $RPC | grep -q "\"$N64\"" && ok "64-byte name stored" || bad "64-byte name"
send "$INIT" 101 "金库 🦊" $K0 >/dev/null
SAFE101=$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $F $CODE $S "$INIT" 101 --rpc-url $RPC)
cast call $R "genesisOf(address)(bool,$GT)" $SAFE101 --rpc-url $RPC | grep -q '金库 🦊' && ok "UTF-8 name round-trips" || bad "UTF-8 name"
# 21 chars of 3-byte UTF-8 = 63 bytes ok; 22 = 66 bytes refused (the limit is bytes, not characters)
eq "22 CJK chars (66 bytes) -> NameTooLong(66)" "$(revert_of $F $CODE $S "$INIT" 102 "$(python3 -c "print('金'*22)")")" "$(err 'NameTooLong(uint256)' 66)"
# raw bytes 0xff 0xfe: the ABI string carries them; the chain stores them as is
RAW=$(cast calldata "$SIG_RECORD" $F $CODE $S "$INIT" 103 "zz")
RAW=${RAW%7a7a000000000000000000000000000000000000000000000000000000000000}fffe000000000000000000000000000000000000000000000000000000000000
cast send $R "$RAW" --private-key $K0 --rpc-url $RPC --json 2>/dev/null | jq -r .status | grep -q 0x1 && ok "non-UTF-8 name accepted (as documented: not checked on chain)" || bad "raw-bytes name send"

echo "== 7. bulk: 30 more records from 3 accounts, then pagination"
for i in $(seq 1 30); do
  case $((i % 3)) in 0) K=$K0;; 1) K=$K1;; 2) K=$K2;; esac
  send "$INIT" $((1000 + i)) "Safe $i" $K >/dev/null
done
TOTAL=$(cast call $R 'totalRecords()(uint256)' --rpc-url $RPC)
echo "  totalRecords = $TOTAL"
eq "total = 1 + 3 edge + 30" "$TOTAL" 34
# every index: genesisOf(safeAt(i)).index == i
MISMATCH=0
for i in $(seq 0 $((TOTAL-1))); do
  A=$(cast call $R 'safeAt(uint256)(address)' $i --rpc-url $RPC)
  IDX=$(cast abi-decode --input "f(bool,$GT)" "$(cast call $R 'genesisOf(address)' $A --rpc-url $RPC)" | sed -n 2p | python3 -c "import sys,re; t=sys.stdin.read(); print(re.findall(r'\d+', t.split(',')[8])[0])")
  [ "$IDX" == "$i" ] || { MISMATCH=$((MISMATCH+1)); echo "    index $i: record says $IDX"; }
done
eq "genesisOf(safeAt(i)).index == i for all $TOTAL" "$MISMATCH" 0

page() { cast call $R 'genesisPage(uint256,uint256,bool)' "$1" "$2" "$3" --rpc-url $RPC; }
page_safes() { cast abi-decode --input "f(uint256,address[],$GT[])" "$(page "$@")" | sed -n 2p; }
expect_safes() { # offset limit desc -> list via safeAt
  local out="" n=$TOTAL c=0
  for ((j=0; j<$2; j++)); do
    if [ "$3" == true ]; then k=$((n-1-$1-j)); else k=$(($1+j)); fi
    [ $k -ge 0 ] && [ $k -lt $n ] || break
    out="$out, $(cast call $R 'safeAt(uint256)(address)' $k --rpc-url $RPC)"
  done
  echo "[${out#, }]"
}
for args in "0 20 true" "20 20 true" "0 20 false" "33 5 false" "30 10 true" "0 100 true"; do
  eq "genesisPage($args) == safeAt walk" "$(page_safes $args)" "$(expect_safes $args)"
done
eq "page past the end is empty" "$(page_safes 34 20 true)" "[]"
eq "limit 0 is empty" "$(page_safes 0 0 true)" "[]"
eq "max offset+limit: empty, no revert" "$(page_safes 115792089237316195423570985008687907853269984665640564039457584007913129639935 115792089237316195423570985008687907853269984665640564039457584007913129639935 true)" "[]"
eq "page total" "$(cast abi-decode --input "f(uint256,address[],$GT[])" "$(page 0 1 true)" | sed -n 1p)" 34
# a page's records are byte-identical to genesisOf
P1=$(cast call $R 'genesisPage(uint256,uint256,bool)' 0 1 true --rpc-url $RPC)
NEWEST=$(cast call $R 'safeAt(uint256)(address)' 33 --rpc-url $RPC)
G1=$(cast abi-decode --input "f(bool,$GT)" "$(cast call $R 'genesisOf(address)' $NEWEST --rpc-url $RPC)" | sed -n 2p)
PG1=$(cast abi-decode --input "f(uint256,address[],$GT[])" "$P1" | sed -n 3p)
eq "page record == genesisOf (newest)" "$PG1" "[$G1]"

echo "== 8. eth_call cost of pages (fresh call = cold storage)"
for n in 20 34; do
  echo "  genesisPage(0,$n,true): $(cast estimate $R 'genesisPage(uint256,uint256,bool)' 0 $n true --rpc-url $RPC) gas (incl. 21k base)"
done

echo "== 9. replay: deployCallOf deploys the recorded Safe at its address"
read TO DATA_ <<<"$(cast call $R 'deployCallOf(address)(address,bytes)' $SAFE --rpc-url $RPC | tr '\n' ' ')"
eq "deployCallOf.to = factory" "$TO" "$F"
eq "no code at the Safe yet" "$(cast code $SAFE --rpc-url $RPC)" 0x
cast send $TO "$DATA_" --private-key $K0 --rpc-url $RPC >/dev/null
[ "$(cast code $SAFE --rpc-url $RPC)" != 0x ] && ok "Safe deployed at the recorded address" || bad "no code at $SAFE"
eq "deployed Safe's owners" "$(cast call $SAFE 'getOwners()(address[])' --rpc-url $RPC)" "[$A0, $A1, $A2]"
eq "deployed Safe's threshold" "$(cast call $SAFE 'getThreshold()(uint256)' --rpc-url $RPC)" 2

echo "== 9b. rename: only the Safe itself, only the name"
G_BEFORE=$(cast call $R 'genesisOf(address)' $SAFE --rpc-url $RPC)
eq "a stranger: NotRecorded(stranger)" "$(view_revert_from $A2 "$(cast calldata 'rename(string)' Mine)")" "$(err 'NotRecorded(address)' $A2)"
eq "the recorder: NotRecorded(recorder)" "$(view_revert_from $A1 "$(cast calldata 'rename(string)' Mine)")" "$(err 'NotRecorded(address)' $A1)"
cast rpc anvil_impersonateAccount $SAFE --rpc-url $RPC >/dev/null
cast rpc anvil_setBalance $SAFE 0xde0b6b3a7640000 --rpc-url $RPC >/dev/null
eq "the Safe, empty name: EmptyName" "$(view_revert_from $SAFE "$(cast calldata 'rename(string)' '')")" "$(err 'EmptyName()')"
RN=$(cast send $R 'rename(string)' "Family vault" --from $SAFE --unlocked --rpc-url $RPC --json)
RN_BLOCK=$(cast to-dec "$(echo "$RN" | jq -r .blockNumber)")
eq "the Safe renames itself" "$(echo "$RN" | jq -r .status)" 0x1
echo "  gas used (rename): $(cast to-dec "$(echo "$RN" | jq -r .gasUsed)")"
eq "GenesisRenamed topics" "$(echo "$RN" | jq -r '.logs[0].topics | join(" ")')" "$(cast keccak 'GenesisRenamed(address,string)') $(cast abi-encode 'f(address)' $SAFE)"
eq "GenesisRenamed data" "$(echo "$RN" | jq -r '.logs[0].data')" "$(cast abi-encode 'f(string)' 'Family vault')"
FIELDS=$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC --json | jq -c '.[1]')
eq "name is the Safe's" "$(echo "$FIELDS" | jq -r '.[9]')" "Family vault"
eq "renamedAt = the rename's block" "$(echo "$FIELDS" | jq -r '.[2]')" "$RN_BLOCK"
P1=$(slotp $BASE 1)
eq "slot+1 renamedAt packed beside singleton" "$(cast to-dec 0x${P1:10:16})" "$RN_BLOCK"
eq "slot+1 singleton untouched" "0x${P1:26:40}" "$S"
# every other field is as it was: re-encode the old record with the new name and block
BEFORE_JSON=$(cast abi-decode --input "f(bool,$GT)" "$G_BEFORE" --json | jq -c '.[1]')
eq "only name and renamedAt changed" "$(echo "$BEFORE_JSON" | jq -c --arg b "$RN_BLOCK" '.[2] = ($b|tonumber) | .[9] = "Family vault"')" "$(echo "$FIELDS" | jq -c '.[2] |= tonumber')"
eq "total unchanged" "$(cast call $R 'totalRecords()(uint256)' --rpc-url $RPC)" 34
cast send $R 'rename(string)' "Renamed twice" --from $SAFE --unlocked --rpc-url $RPC >/dev/null
eq "renames again" "$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC --json | jq -r '.[1][9]')" "Renamed twice"
cast rpc anvil_stopImpersonatingAccount $SAFE --rpc-url $RPC >/dev/null
send "$INIT" 7 "Squat again" $K2 >/dev/null
eq "a repeat write keeps the Safe's name" "$(cast call $R "genesisOf(address)(bool,$GT)" $SAFE --rpc-url $RPC --json | jq -r '.[1][9]')" "Renamed twice"
eq "the page shows the Safe's name" "$(cast call $R "genesisPage(uint256,uint256,bool)(uint256,address[],$GT[])" 33 1 true --rpc-url $RPC --json | jq -r '.[2][0][9]')" "Renamed twice"

eq "deployCallOf(unrecorded) empty" "$(cast call $R 'deployCallOf(address)(address,bytes)' 0x000000000000000000000000000000000000dEaD --rpc-url $RPC | tr '\n' ' ')" "0x0000000000000000000000000000000000000000 0x "
eq "genesisOf(unrecorded).found" "$(cast call $R "genesisOf(address)(bool,$GT)" 0x000000000000000000000000000000000000dEaD --rpc-url $RPC | head -1)" false


if [ -n "${FORK_RPC_GNOSIS:-}" ]; then
  echo "== 10. real Safe v1.4.1 on a Gnosis fork: record, replay, inspect"
  FPORT=$((PORT + 1)); FRPC=http://127.0.0.1:$FPORT
  start_anvil $FPORT "$FORK_RPC_GNOSIS"
  FACT=0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67   # SafeProxyFactory 1.4.1
  SING=0x29fcB43b46531BcA003ddC8FCB67FFE91900C762   # SafeL2 1.4.1
  FH=0xfd0732Dc9E303f09fCEf3a7388Ad10A83459Ec99     # CompatibilityFallbackHandler 1.4.1
  eq "same registry address on the fork" "$(deploy_registry $FRPC)" "$R"
  O1=0x1111111111111111111111111111111111111111; O2=0x2222222222222222222222222222222222222222
  RINIT=$(cast calldata 'setup(address[],uint256,address,bytes,address,address,uint256,address)' "[$O1,$O2,$A1]" 1 $Z 0x $FH $Z 0 $Z)
  RCODE=$(cast call $FACT 'proxyCreationCode()(bytes)' --rpc-url $FRPC)
  RNONCE=$(cast keccak "safe-genesis-registry/e2e")
  RSAFE=$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $FACT $RCODE $SING $RINIT $RNONCE --rpc-url $FRPC)
  eq "registry address == the real factory's" "$RSAFE" "$(cast call $FACT 'createProxyWithNonce(address,bytes,uint256)(address)' $SING $RINIT $RNONCE --rpc-url $FRPC)"
  RTX=$(cast send $R "$SIG_RECORD" $FACT $RCODE $SING $RINIT $RNONCE "Gnosis Team" --private-key $K1 --rpc-url $FRPC --json)
  eq "record on the fork" "$(echo "$RTX" | jq -r .status)" 0x1
  echo "  gas used (real Safe, 3 owners): $(cast to-dec "$(echo "$RTX" | jq -r .gasUsed)")"
  read RTO RDATA <<<"$(cast call $R 'deployCallOf(address)(address,bytes)' $RSAFE --rpc-url $FRPC | tr '\n' ' ')"
  eq "replay" "$(cast send $RTO $RDATA --private-key $K0 --rpc-url $FRPC --json | jq -r .status)" 0x1
  eq "replayed VERSION" "$(cast call $RSAFE 'VERSION()(string)' --rpc-url $FRPC)" '"1.4.1"'
  eq "replayed owners" "$(cast call $RSAFE 'getOwners()(address[])' --rpc-url $FRPC)" "[$O1, $O2, $A1]"
  eq "replayed threshold" "$(cast call $RSAFE 'getThreshold()(uint256)' --rpc-url $FRPC)" 1
  eq "replayed fallback handler" "$(cast storage $RSAFE 0x6c9a6c4a39284e37ed1cf53d337577d14212a4870fb976a4366c693b939918d5 --rpc-url $FRPC)" "$(cast abi-encode 'f(address)' $FH)"
  eq "replayed singleton" "$(cast storage $RSAFE 0 --rpc-url $FRPC)" "$(cast abi-encode 'f(address)' $SING)"
  RX=$(safe_exec $FRPC $RSAFE $R "$(cast calldata 'rename(string)' 'Renamed by its owners')" $K1)
  eq "the real Safe renames itself (execTransaction)" "$(echo "$RX" | jq -r .status)" 0x1
  eq "its name is now its own" "$(cast call $R "genesisOf(address)(bool,$GT)" $RSAFE --rpc-url $FRPC --json | jq -r '.[1][9]')" "Renamed by its owners"
  eq "renamedAt = that block" "$(cast call $R "genesisOf(address)(bool,$GT)" $RSAFE --rpc-url $FRPC --json | jq -r '.[1][2]')" "$(cast to-dec "$(echo "$RX" | jq -r .blockNumber)")"
else
  echo "== 10. (skipped: set FORK_RPC_GNOSIS for the real Safe v1.4.1 fork check)"
fi

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
