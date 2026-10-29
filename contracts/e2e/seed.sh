#!/usr/bin/env bash
# Fill a deployed registry with varied, real Safe genesis records: for a demo
# page, and for readers to be tried against data they did not write.
#
# Every record is a real Safe creation (v1.3.0, v1.4.1, v1.5.0; Safe and
# SafeL2) whose address the real factory agrees with; some are also deployed
# from deployCallOf. Three recorders write them (the key given, and two
# accounts derived from it and funded by it). Names cover repeats, CJK, emoji,
# accents, exactly 64 bytes, and two hostile ones a reader must survive: an
# HTML injection probe and bytes that are not UTF-8.
#
#   RPC=https://rpc.testnet.arc.network PRIVATE_KEY=0x... contracts/e2e/seed.sh
#
# Records are permanent; each run adds a new set (salt nonces include the time).
set -uo pipefail
cd "$(dirname "$0")/.."

: "${RPC:?set RPC}" "${PRIVATE_KEY:?set PRIVATE_KEY}"
R=${REGISTRY:-0x535fE27b2b52EfbEFFfE2409890C2A00e7FF801E}
Z=0x0000000000000000000000000000000000000000
SIG_RECORD='record(address,bytes,address,bytes,uint256,string)'
RUN=$(date -u +%Y%m%dT%H%M%SZ)

# version -> factory, Safe, SafeL2, fallback handler (bash 3.2: no associative arrays)
factory_of() { case $1 in 1.3.0) echo 0xa6B71E26C5e0845f74c812102Ca7114b6a896AB2;; 1.4.1) echo 0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67;; 1.5.0) echo 0x14F2982D601c9458F93bd70B218933A6f8165e7b;; esac; }
safe_of()    { case $1 in 1.3.0) echo 0xd9Db270c1B5E3Bd161E8c8503c55cEABeE709552;; 1.4.1) echo 0x41675C099F32341bf84BFc5382aF534df5C7461a;; 1.5.0) echo 0xFf51A5898e281Db6DfC7855790607438dF2ca44b;; esac; }
safe_l2_of() { case $1 in 1.3.0) echo 0x3E5c63644E683549055b9Be8653de26E0B4CD36E;; 1.4.1) echo 0x29fcB43b46531BcA003ddC8FCB67FFE91900C762;; 1.5.0) echo 0xEdd160fEBBD92E350D4D398fb636302fccd67C7e;; esac; }
handler_of() { case $1 in 1.3.0) echo 0xf48f2B2d2a534e402487b3ee7C18c33Aec0Fe5e4;; 1.4.1) echo 0xfd0732Dc9E303f09fCEf3a7388Ad10A83459Ec99;; 1.5.0) echo 0x3EfCBb83A4A7AfcB4F68D501E2c2203a38be77f4;; esac; }
for v in 1.3.0 1.4.1 1.5.0; do eval "CODE_${v//./_}=$(cast call "$(factory_of $v)" 'proxyCreationCode()(bytes)' --rpc-url "$RPC")"; done
code_of() { eval echo "\$CODE_${1//./_}"; }

# Recorders: the key given, and two derived from it (same accounts every run).
KEYS=("$PRIVATE_KEY" "$(cast keccak "recorder/1/${PRIVATE_KEY}")" "$(cast keccak "recorder/2/${PRIVATE_KEY}")")
ADDRS=(); for k in "${KEYS[@]}"; do ADDRS+=("$(cast wallet address "$k")"); done
for i in 1 2; do
  bal=$(cast balance "${ADDRS[$i]}" --rpc-url "$RPC")
  if [ "$(python3 -c "print(int($bal) < 10**17)")" == True ]; then
    cast send "${ADDRS[$i]}" --value 0.2ether --private-key "$PRIVATE_KEY" --rpc-url "$RPC" >/dev/null && echo "funded recorder $i ${ADDRS[$i]}"
  fi
done

owner() { cast wallet address "$(cast keccak "seed/$RUN/$1/owner/$2")"; }

# name | version | Safe or SafeL2 | owners | threshold | recorder | deploy it too
PLACEHOLDER_NON_UTF8='ZZZZZZZZZ'   # 9 bytes, swapped below for "Bad \xff\xfe ok"
COUNT=0; FAILED=0
while IFS='|' read -r NAME V KIND OWNERS THRESHOLD WHO DEPLOY; do
  [ -z "$NAME" ] && continue
  COUNT=$((COUNT+1))
  F=$(factory_of $V); CODE=$(code_of $V)
  SING=$(safe_l2_of $V); [ "$KIND" == Safe ] && SING=$(safe_of $V)
  ME=${ADDRS[$WHO]}; KEY=${KEYS[$WHO]}
  LIST=$ME; for ((o=1; o<OWNERS; o++)); do LIST="$LIST,$(owner $COUNT $o)"; done
  INIT=$(cast calldata 'setup(address[],uint256,address,bytes,address,address,uint256,address)' "[$LIST]" $THRESHOLD $Z 0x $(handler_of $V) $Z 0 $Z)
  NONCE=$(cast keccak "seed/$RUN/$COUNT")
  DATA=$(cast calldata "$SIG_RECORD" $F $CODE $SING $INIT $NONCE "$NAME")
  if [ "$NAME" == "$PLACEHOLDER_NON_UTF8" ]; then DATA=${DATA/5a5a5a5a5a5a5a5a5a/42616420fffe206f6b}; fi
  SAFE=$(cast call $R 'computeAddress(address,bytes,address,bytes,uint256)(address)' $F $CODE $SING $INIT $NONCE --rpc-url "$RPC")
  STATUS=$(cast send $R "$DATA" --private-key "$KEY" --rpc-url "$RPC" --json | jq -r .status)
  [ "$STATUS" == 0x1 ] || FAILED=$((FAILED+1))
  LINE="#$COUNT $SAFE v$V $KIND $THRESHOLD/$OWNERS by recorder $WHO: $STATUS"
  if [ "$DEPLOY" == y ]; then
    read TO CALL <<<"$(cast call $R 'deployCallOf(address)(address,bytes)' $SAFE --rpc-url "$RPC" | tr '\n' ' ')"
    DSTATUS=$(cast send $TO $CALL --private-key "$KEY" --rpc-url "$RPC" --json | jq -r .status)
    [ "$DSTATUS" == 0x1 ] || FAILED=$((FAILED+1))
    LINE="$LINE, deployed: $DSTATUS"
  fi
  echo "$LINE  [$NAME]"
done <<'EOF'
Family Vault|1.4.1|SafeL2|3|2|0|y
Team Treasury|1.4.1|SafeL2|5|3|1|n
Team Treasury|1.3.0|SafeL2|3|2|2|n
Ops Multisig|1.5.0|SafeL2|4|2|0|y
Cold Storage|1.4.1|Safe|5|3|1|n
Payroll|1.3.0|Safe|1|1|2|y
DAO Council|1.5.0|SafeL2|7|4|0|n
Grant Committee|1.4.1|SafeL2|4|3|1|n
金库|1.4.1|SafeL2|2|1|2|y
开发基金 🚀|1.5.0|SafeL2|3|2|0|n
財務チーム|1.3.0|SafeL2|2|2|1|n
Treasury — Ops (EU)|1.4.1|Safe|3|2|2|n
A name of exactly sixty-four bytes, the longest the registry tak|1.4.1|SafeL2|2|2|0|n
Alice|1.4.1|SafeL2|1|1|1|y
Bob & Carol|1.4.1|SafeL2|2|2|2|n
<img src=x onerror=alert(1)>|1.4.1|SafeL2|1|1|0|n
ZZZZZZZZZ|1.4.1|SafeL2|1|1|1|n
Market Making|1.5.0|Safe|3|2|2|n
Bridge Reserve|1.3.0|SafeL2|7|5|0|y
Savings 💰|1.4.1|SafeL2|1|1|1|n
Investment Club|1.4.1|SafeL2|6|3|2|n
Hackathon Prize Pool|1.5.0|SafeL2|3|2|0|n
Escrow #42|1.4.1|SafeL2|2|2|1|y
Security Council|1.4.1|SafeL2|9|6|2|n
Charity Fund|1.3.0|Safe|3|2|0|n
Ops Multisig|1.4.1|SafeL2|3|2|1|n
Test 1-of-1|1.5.0|SafeL2|1|1|2|y
Shared Wallet|1.4.1|SafeL2|2|1|0|n
Résumé Café|1.4.1|SafeL2|3|2|1|n
Sandbox|1.3.0|SafeL2|1|1|2|n
EOF
echo "seeded $COUNT records, $FAILED failed writes; registry total $(cast call $R 'totalRecords()(uint256)' --rpc-url "$RPC")"
[ "$FAILED" -eq 0 ]
