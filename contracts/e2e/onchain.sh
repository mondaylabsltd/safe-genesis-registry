#!/usr/bin/env bash
# On-chain check of a deployed SafeGenesisRegistry, with the real Safe v1.4.1.
#
# Read-only part (no key needed): the deployed code is this build, every
# revert with its arguments (eth_call), the address formula against the real
# factory, and the consistency of every record already there: index,
# safeAt, genesisOf and genesisPage agree.
#
# With PRIVATE_KEY, it also writes: a real Safe v1.4.1 genesis under a name
# "E2E check <time>", a repeat write, a mined revert, then deploys the Safe
# from deployCallOf and inspects it. Records are permanent: each run adds one
# record to the registry's public list, and one Safe to the chain.
#
#   RPC=https://rpc.testnet.arc.network contracts/e2e/onchain.sh
#   RPC=... PRIVATE_KEY=0x... contracts/e2e/onchain.sh
#
# Needs forge, cast, jq and python3; the chain must have Safe v1.4.1 at its
# canonical addresses. Exits non-zero on any failure.
set -uo pipefail
cd "$(dirname "$0")/.."

: "${RPC:?set RPC}"
R=${REGISTRY:-0x535fE27b2b52EfbEFFfE2409890C2A00e7FF801E}
FACT=0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67   # SafeProxyFactory 1.4.1
SING=0x29fcB43b46531BcA003ddC8FCB67FFE91900C762   # SafeL2 1.4.1
FH=0xfd0732Dc9E303f09fCEf3a7388Ad10A83459Ec99     # CompatibilityFallbackHandler 1.4.1
Z=0x0000000000000000000000000000000000000000
FALLBACK_HANDLER_SLOT=0x6c9a6c4a39284e37ed1cf53d337577d14212a4870fb976a4366c693b939918d5
GT='(address,address,uint256,bytes,bytes32,address,uint64,uint32,string)'
SIG_RECORD='record(address,bytes,address,bytes,uint256,string)'

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL  $*"; }
eq()  { if [ "$(echo "$2" | tr A-F a-f)" == "$(echo "$3" | tr A-F a-f)" ]; then ok "$1"; else bad "$1: got [$2] want [$3]"; fi; }
err() { cast calldata "$@"; }
# full revert data (selector + args) of an eth_call with this calldata
revert_data() {
  cast rpc eth_call "{\"to\":\"$R\",\"data\":\"$1\"}" latest --rpc-url "$RPC" 2>&1 \
    | grep -oE '0x[0-9a-f]{8}([0-9a-f]{64})*"?' | tail -1 | tr -d '"'
}
total() { cast call $R 'totalRecords()(uint256)' --rpc-url "$RPC" | awk '{print $1}'; }
genesis_json() { cast call $R "genesisOf(address)(bool,$GT)" "$1" --rpc-url "$RPC" --json; }
field() { genesis_json "$1" | jq -r ".[1][$2]"; } # 0 factory .. 7 index, 8 name

echo "== chain $(cast chain-id --rpc-url "$RPC"), registry $R"
forge build -q

echo "== 1. the deployed code"
eq "runtime code == this build" "$(cast code $R --rpc-url "$RPC")" "$(forge inspect SafeGenesisRegistry deployedBytecode)"
eq "VERSION" "$(cast call $R 'VERSION()(uint8)' --rpc-url "$RPC")" 3
for a in $FACT $SING $FH; do [ "$(cast code $a --rpc-url "$RPC")" != 0x ] && ok "Safe 1.4.1 contract at $a" || bad "no code at $a"; done

echo "== 2. refusals (eth_call, nothing written)"
CODE=$(cast call $FACT 'proxyCreationCode()(bytes)' --rpc-url "$RPC")
INIT=$(cast calldata 'setup(address[],uint256,address,bytes,address,address,uint256,address)' \
  "[0x1111111111111111111111111111111111111111]" 1 $Z 0x $FH $Z 0 $Z)
rec() { cast calldata "$SIG_RECORD" "$@"; }
eq "empty name -> EmptyName"               "$(revert_data "$(rec $FACT $CODE $SING $INIT 1 "")")"                          "$(err 'EmptyName()')"
eq "65-byte name -> NameTooLong(65)"       "$(revert_data "$(rec $FACT $CODE $SING $INIT 1 "$(printf 'x%.0s' $(seq 65))")")" "$(err 'NameTooLong(uint256)' 65)"
eq "zero factory -> ZeroAddress"           "$(revert_data "$(rec $Z $CODE $SING $INIT 1 n)")"                              "$(err 'ZeroAddress()')"
eq "zero singleton -> ZeroAddress"         "$(revert_data "$(rec $FACT $CODE $Z $INIT 1 n)")"                              "$(err 'ZeroAddress()')"
eq "empty code -> EmptyProxyCreationCode"  "$(revert_data "$(rec $FACT 0x $SING $INIT 1 n)")"                              "$(err 'EmptyProxyCreationCode()')"
eq "empty init -> EmptyInitializer"        "$(revert_data "$(rec $FACT $CODE $SING 0x 1 n)")"                              "$(err 'EmptyInitializer()')"
eq "8193-byte init -> InitializerTooLong"  "$(revert_data "$(rec $FACT $CODE $SING 0x$(python3 -c "print('ab'*8193)") 1 n)")" "$(err 'InitializerTooLong(uint256)' 8193)"
N=$(total)
eq "safeAt(total) -> IndexOutOfBounds" "$(revert_data "$(cast calldata 'safeAt(uint256)' $N)")" "$(err 'IndexOutOfBounds(uint256,uint256)' $N $N)"
eq "computeAddress == the real factory's" \
  "$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $FACT $CODE $SING $INIT 1 --rpc-url "$RPC")" \
  "$(cast call $FACT 'createProxyWithNonce(address,bytes,uint256)(address)' $SING $INIT 1 --rpc-url "$RPC")"

check_registry() { # every record's index, safeAt, and pages agree (up to 200 records)
  local n; n=$(total); echo "  $n records"
  local upto=$(( n < 200 ? n : 200 )) bad_index=0 walk=""
  for ((i=0; i<upto; i++)); do
    local s; s=$(cast call $R 'safeAt(uint256)(address)' $i --rpc-url "$RPC")
    [ "$(field $s 7)" == "$i" ] || { bad_index=$((bad_index+1)); echo "    index $i: record says $(field $s 7)"; }
    walk="$s, $walk"
  done
  eq "genesisOf(safeAt(i)).index == i for $upto records" "$bad_index" 0
  if [ "$n" -le 200 ]; then
    walk="[${walk%, }]"
    eq "genesisPage(0, all, newest first) == safeAt walk" \
      "$(cast abi-decode --input "f(uint256,address[],$GT[])" "$(cast call $R 'genesisPage(uint256,uint256,bool)' 0 $n true --rpc-url "$RPC")" | sed -n 2p)" "$walk"
  fi
  eq "page past the end is empty" \
    "$(cast abi-decode --input "f(uint256,address[],$GT[])" "$(cast call $R 'genesisPage(uint256,uint256,bool)' $n 20 true --rpc-url "$RPC")" | sed -n 2p)" "[]"
  local max=115792089237316195423570985008687907853269984665640564039457584007913129639935
  eq "max offset and limit: empty, no revert" \
    "$(cast abi-decode --input "f(uint256,address[],$GT[])" "$(cast call $R 'genesisPage(uint256,uint256,bool)' $max $max true --rpc-url "$RPC")" | sed -n 2p)" "[]"
  [ "$n" -gt 0 ] && echo "  eth_call gas, genesisPage(0, $(( n < 100 ? n : 100 )), true): $(cast estimate $R 'genesisPage(uint256,uint256,bool)' 0 $(( n < 100 ? n : 100 )) true --rpc-url "$RPC")"
}
echo "== 3. the records already there"
check_registry

if [ -z "${PRIVATE_KEY:-}" ]; then
  echo "== 4-6. (skipped: set PRIVATE_KEY to write and replay)"
else
  ME=$(cast wallet address "$PRIVATE_KEY")
  echo "== 4. write a real Safe v1.4.1 genesis, from $ME (balance $(cast balance $ME --ether --rpc-url "$RPC"))"
  O1=0x1111111111111111111111111111111111111111; O2=0x2222222222222222222222222222222222222222
  INIT=$(cast calldata 'setup(address[],uint256,address,bytes,address,address,uint256,address)' "[$ME,$O1,$O2]" 2 $Z 0x $FH $Z 0 $Z)
  STAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  NONCE=$(cast keccak "safe-genesis-registry/e2e/$ME/$STAMP")
  NAME="E2E check $STAMP"
  BEFORE=$(total)
  read SAFE CREATED <<<"$(cast call $R "$SIG_RECORD(address,bool)" $FACT $CODE $SING $INIT $NONCE "$NAME" --from $ME --rpc-url "$RPC" | tr '\n' ' ')"
  eq "simulated: created" "$CREATED" true
  eq "simulated: the real factory's address" "$SAFE" "$(cast call $FACT 'createProxyWithNonce(address,bytes,uint256)(address)' $SING $INIT $NONCE --rpc-url "$RPC")"
  RCPT=$(cast send $R "$SIG_RECORD" $FACT $CODE $SING $INIT $NONCE "$NAME" --private-key "$PRIVATE_KEY" --rpc-url "$RPC" --json)
  TX=$(echo "$RCPT" | jq -r .transactionHash); BLOCK=$(cast to-dec "$(echo "$RCPT" | jq -r .blockNumber)")
  echo "  record tx $TX, block $BLOCK, gas $(cast to-dec "$(echo "$RCPT" | jq -r .gasUsed)") — Safe $SAFE"
  eq "tx status" "$(echo "$RCPT" | jq -r .status)" 0x1
  eq "one log" "$(echo "$RCPT" | jq '.logs | length')" 1
  eq "log topics" "$(echo "$RCPT" | jq -r '.logs[0].topics | join(" ")')" \
    "$(cast keccak 'GenesisRecorded(address,address,address,uint256,bytes32,bytes,string)') $(cast abi-encode 'f(address)' $SAFE) $(cast abi-encode 'f(address)' $FACT) $(cast abi-encode 'f(address)' $SING)"
  eq "log data" "$(echo "$RCPT" | jq -r '.logs[0].data')" "$(cast abi-encode 'f(uint256,bytes32,bytes,string)' $NONCE $(cast keccak $CODE) $INIT "$NAME")"
  eq "genesisOf: factory"    "$(field $SAFE 0)" "$FACT"
  eq "genesisOf: singleton"  "$(field $SAFE 1)" "$SING"
  eq "genesisOf: initializer" "$(field $SAFE 3)" "$INIT"
  eq "genesisOf: code hash"  "$(field $SAFE 4)" "$(cast keccak $CODE)"
  eq "genesisOf: recorder"   "$(field $SAFE 5)" "$ME"
  eq "genesisOf: recordedAt" "$(field $SAFE 6)" "$BLOCK"
  eq "genesisOf: name"       "$(field $SAFE 8)" "$NAME"
  IDX=$(field $SAFE 7)
  [ "$IDX" -ge "$BEFORE" ] && ok "index $IDX follows the $BEFORE records before it" || bad "index $IDX < $BEFORE"
  eq "safeAt(index)" "$(cast call $R 'safeAt(uint256)(address)' $IDX --rpc-url "$RPC")" "$SAFE"
  NOW=$(total)
  eq "newest-first page holds it at total-1-index" \
    "$(cast abi-decode --input "f(uint256,address[],$GT[])" "$(cast call $R 'genesisPage(uint256,uint256,bool)' $((NOW-1-IDX)) 1 true --rpc-url "$RPC")" | sed -n 2p)" "[$SAFE]"

  echo "== 5. a repeat write, and a mined revert"
  RCPT2=$(cast send $R "$SIG_RECORD" $FACT $CODE $SING $INIT $NONCE "Another name" --private-key "$PRIVATE_KEY" --rpc-url "$RPC" --json)
  eq "repeat: tx ok" "$(echo "$RCPT2" | jq -r .status)" 0x1
  eq "repeat: no log" "$(echo "$RCPT2" | jq '.logs | length')" 0
  eq "repeat: name kept" "$(field $SAFE 8)" "$NAME"
  eq "repeat: index kept" "$(field $SAFE 7)" "$IDX"
  NONCE2=$(cast keccak "reverted:$NONCE")
  FAILED=$(cast send $R "$SIG_RECORD" $FACT $CODE $SING $INIT $NONCE2 "" --gas-limit 300000 --private-key "$PRIVATE_KEY" --rpc-url "$RPC" --json 2>/dev/null)
  eq "mined revert: status 0" "$(echo "$FAILED" | jq -r .status)" 0x0
  eq "mined revert: nothing recorded" \
    "$(cast call $R 'isRecorded(address)(bool)' "$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $FACT $CODE $SING $INIT $NONCE2 --rpc-url "$RPC")" --rpc-url "$RPC")" false

  echo "== 6. replay: deploy the Safe from deployCallOf, then inspect it"
  read TO DATA <<<"$(cast call $R 'deployCallOf(address)(address,bytes)' $SAFE --rpc-url "$RPC" | tr '\n' ' ')"
  eq "deployCallOf.to" "$TO" "$FACT"
  eq "no code at the Safe before" "$(cast code $SAFE --rpc-url "$RPC")" 0x
  DEPLOY=$(cast send $TO $DATA --private-key "$PRIVATE_KEY" --rpc-url "$RPC" --json)
  echo "  deploy tx $(echo "$DEPLOY" | jq -r .transactionHash)"
  eq "deploy status" "$(echo "$DEPLOY" | jq -r .status)" 0x1
  eq "Safe VERSION"   "$(cast call $SAFE 'VERSION()(string)' --rpc-url "$RPC")" '"1.4.1"'
  eq "Safe owners"    "$(cast call $SAFE 'getOwners()(address[])' --rpc-url "$RPC")" "[$ME, $O1, $O2]"
  eq "Safe threshold" "$(cast call $SAFE 'getThreshold()(uint256)' --rpc-url "$RPC")" 2
  eq "Safe fallback handler" "$(cast storage $SAFE $FALLBACK_HANDLER_SLOT --rpc-url "$RPC")" "$(cast abi-encode 'f(address)' $FH)"
  eq "Safe singleton" "$(cast storage $SAFE 0 --rpc-url "$RPC")" "$(cast abi-encode 'f(address)' $SING)"

  echo "== 7. the registry after the writes"
  check_registry
fi

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
