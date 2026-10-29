# genesis-cf — the Safe Genesis Registry service

The registry's companion service on Cloudflare Workers. Every decision it
makes comes from [`genesis-rules`](../genesis-rules); this crate only fetches
what a rule asks for and acts on what it returns.

The registry needs no service to be correct: it is self-verifying, anyone can
write to it, and anyone can read it with `eth_call`. The service adds what a
wallet should not have to do itself: one call instead of a scan of every
chain, the replay safety check, per-chain member drift, and sponsored imports.

| Piece | Where |
| --- | --- |
| Lookups, status, replay checks, import resolution | the stateless Worker (`src/edge.rs`) |
| The sponsored-import queue | one Durable Object, tasks in its SQLite, driven by its alarm (`src/submitter.rs`) |
| Chain access | `fetch` JSON-RPC with endpoint failover (`src/rpc.rs`) |
| Response cache | the platform Cache API, keys and TTLs from `genesis_rules::cache` |
| Operator alerts | the cron tick runs the Durable Object's maintenance pass; `genesis_rules::sentinel` decides; Telegram delivers |

The same service also runs as a single container: [`genesis-server`](../genesis-server).

## API

All responses are JSON; addresses in paths may be in any case.

Conventions, the same on every route:

- An error is `{"error": "<message>"}`; a request the rules refuse (an import,
  a proposal) is `422 {"refusal": {…}}`.
- `404` means every registry answered that it holds nothing. When a registry
  could not be read (an RPC down, or the configured address holding no
  registry or another version of it), the answer is `502`, never `404`.
- Chain ids, block numbers, indexes and counts are JSON numbers; `uint256`
  values (salt nonces, thresholds, Safe nonces) are `0x` hex strings.

The registry lives on the chains marked `registry` in `CHAINS` (Gnosis and
Ethereum) and on the home chain; lookups read only those. The other chains are
read for deployments, replays and members.

### `GET /api/genesis/{safe}`

The Safe's genesis from whichever registries hold it (home chain preferred).
`verified` re-derives the address from the record and the factory's proxy
code on the chain it was read from. The genesis is the same in every
registry; the name, `index` and dates are each registry's own, listed under
`registries`. `nameBySafe` says whether a name is the Safe's own (it renamed
itself, or wrote the record); otherwise the first writer chose it and a
wallet should say so.

```json
{
  "safe": "0x0f9a…0725", "found": true,
  "recordedOn": [100], "readFrom": 100, "verified": true,
  "genesis": {"factory": "0x…", "singleton": "0x…", "saltNonce": "0x…", "initializer": "0x…", "name": "Team fund"},
  "proxyCreationCodeHash": "0x…", "recorder": "0x…", "recordedAt": 48965199,
  "index": 41, "renamedAt": 0, "nameBySafe": false,
  "registries": [{"chainId": 100, "name": "Team fund", "nameBySafe": false, "renamedAt": 0,
                  "index": 41, "recorder": "0x…", "recordedAt": 48965199}],
  "setup": {"owners": ["0x…"], "threshold": "0x3", "to": "0x…", "data": "0x", "fallbackHandler": "0x…", …},
  "setupDefects": [],
  "deployCall": {"to": "<factory>", "data": "<createProxyWithNonce calldata>"}
}
```

`404` with `"found": false` when no registry holds it; `502` with
`unreachable` when none was found and a registry could not be read.

A Safe renames itself with a Safe transaction `{to: <registry>, data:
rename(name)}` on the registry's chain, proposed and approved like any other
(the mailbox below) and executed by the wallet, which pays for it: the
service never sponsors a rename. The Safe must be deployed there.

### `GET /api/registry/{chainId}?offset=0&limit=20&order=newest|oldest`

One page of a registry's records in write order, newest first by default:
`{chainId, total, offset, limit, order, records: [{safe, index, name,
nameBySafe, renamedAt, recorder, recordedAt, singleton, owners, threshold,
genesis, proxyCreationCodeHash}]}`. `limit` is 1 to 100; `400` for a chain
without a registry. A page the node refuses whole (a gas cap reached by large
initializers) is read in smaller parts. Records written while a page is read
in parts may shift it; `total` says how many there were.

### `GET /api/safe/{safe}/chains`

Every configured chain:

- deployed — the current members and their **drift** from the genesis
  (`{"kind": "same"}`, or `differs` with `added`, `removed`, `thresholdWas`,
  `thresholdIs`);
- deployed — the Safe's current **state** (owners, threshold, modules,
  fallback handler, guard) and its **drift** from what the genesis created
  (`{"kind": "same"}`, or `differs` with owners and modules added/removed,
  threshold, handler and guard changes, and `authorityChanged` when anything
  beyond the owners differs);
- not deployed — the **replay verdict** for that chain, and `controlledBy`:
  who would control funds sent to the address there — stated only when the
  replay is `ready`, because for any other verdict nobody can say.

### `GET /api/replay/{safe}?chainId={id}`

The full replay check for one chain, and the call that deploys the Safe there.

| Verdict | Meaning |
| --- | --- |
| `alreadyDeployed` | The Safe exists there (same address under CREATE2 = same genesis) |
| `ready` | A creation the check reads completely, every contract it touches holding the known code for its role |
| `unverified` | Nothing wrong found, but an owner that is a contract cannot be vouched for (nothing binds its address) |
| `blocked` | Anything else: a setup defect, a setup hook or call the check does not fully decode, a missing dependency, code that is not the known contract for its role, a factory whose proxy code differs |

The check is default-deny: a replay is `ready` only for no setup hook,
`SafeToL2Setup`, `SafeModuleSetup.enableModules`, or a MultiSend whose every
leg is a known (contract kind, operation, function, canonical arguments) —
Vela wallets' passkey setup included. `check.dependencies` lists each contract
with the code observed and the known contract it matched; `check.creates` is
the authority the replay would create (only when `ready`).

### `POST /api/import`

```json
{"chainId": 8453, "safe": "0x…", "name": "Team fund"}
{"chainId": 8453, "safe": "0x…", "name": "Team fund", "safeWalletProps": {…}}   // a Safe{Wallet} Safe deployed nowhere yet
{"chainId": 100,  "safe": "0x…", "genesis": {…, "name": "Team fund"}, "ownerProof": {…}}   // a wallet that just created it
```

Every record carries a name (1 to 64 bytes; a label, not part of the address). A record copied from another chain
keeps its name, and so does a `genesis` sent with one; otherwise `name` is used, and without it the Safe gets a
default name drawn from its address (like "Quiet Harbor": the same on every retry). A `name` the registry would
not take (empty, over 64 bytes) is refused (`unrecordable`), not replaced.

Resolves the Safe's genesis on `chainId`, in this order:

1. from the registry on another chain;
2. from `genesis` (`factory`, `singleton`, `saltNonce`, `initializer`), as sent by the wallet that created the Safe;
3. from Safe{Wallet}'s counterfactual config (`undeployedSafes[chain][safe].props` in a Safe{Wallet} export);
4. from the chain's Safe Transaction Service, and when the service has no parameters, from the creating
   transaction's calldata.

Only parameters that reproduce the Safe's address are accepted.

- `200 {"status": "recorded"}` — the home chain already holds it.
- `202 {"taskId", "status": "queued", "provedBy"}` — verified and queued; the service writes it to the home chain
  at its own expense. Poll `GET /api/task/{taskId}`.
- `200 {"status": "resolved", "recordCall", "reason"}` — not sponsored (see below): the verified `record(...)`
  call, for the caller to send.
- `422 {"refusal"}` — nothing reproduced the address (`unresolved`: `notFound`, `mismatch` with the address the
  evidence gives instead, or `noCreate2` — `createProxy`, as Wintermute's Safe was, or a chain-specific salt), or
  the registry would reject the parameters (`unrecordable`).

**Who pays.** Two lanes, counted separately so a flood in one cannot spend the other:

| Lane | When | Budget |
| --- | --- | --- |
| Deployed | the Safe exists on some chain (its creator paid for a deployment) | 20 per minute, 500 per day |
| Owner | the Safe exists nowhere yet, and `ownerProof` proves an owner asks | 10 per minute, 300 per day, and 5 per owner per day |

Anything else — no sponsor key, a Safe deployed nowhere without a valid proof, a spent budget — gets `resolved`,
and `reason` says which.

**`ownerProof`** — an owner that is a passkey wallet (a Vela account), signing with its passkey:

```json
{
  "owner": "0x…",                // an owner in the Safe's setup
  "ownerGenesis": {"factory", "singleton", "saltNonce", "initializer"},   // the owner wallet's own genesis
  "authenticatorData": "0x…",
  "clientDataJSON": "{\"type\":\"webauthn.get\",\"challenge\":\"…\",…}",   // the exact text that was signed
  "signature": "0x…"             // r ‖ s (64 bytes) or DER
}
```

- The WebAuthn challenge is `keccak256("safe-genesis-registry/sponsor/v1" ‖ safe)`, the 20-byte Safe address.
  `clientDataJSON.challenge` is its base64url form without padding.
- `ownerGenesis` must deploy to `owner`, and set up a wallet that its passkeys control on their own:
  - a setup the replay check reads completely;
  - threshold 1;
  - each key a signer among its owners, either the shared signer `configure`d during setup or a signer the passkey
    factory creates.
- The assertion must be a `webauthn.get` with the user-present flag set, signed by one of those keys.
- Nothing else is read (no index, no chain state beyond the owner factory's proxy code).
- A key costs nothing to make, so the proof is not a cost to the caller. What it does is put these writes in their
  own lane with a per-owner quota. Replaying a proof is harmless: records are write-once.

### The proposal mailbox

Safe transactions waiting for their owners' approvals, for Safes deployed or
not. The mailbox is not a source of trust — a wallet re-hashes, re-reads and
re-verifies everything it gets — but it only takes what the chain would.

`POST /api/proposals`

```json
{"chainId": 100, "safe": "0x…", "safeTx": {"to", "value", "data", "operation", "safeTxGas", "baseGas",
 "gasPrice", "gasToken", "refundReceiver", "nonce"}, "approval": {…}, "genesis": {…}}
```

- `approval` is one owner's, by passkey: `owner`, `ownerGenesis` (it must
  deploy to `owner`), `signer` (one of the owner wallet's passkey signers),
  `authenticatorData`, `clientDataFields`, `r`, `s`. It must verify as the
  owner's on-chain check would: `SafeMessage(safeTxHash)` under the owner's
  domain, `clientDataJSON` rebuilt as the signer contract does,
  user-verified.
- Or by a plain account (an EOA): `{"owner", "signature"}`, its EIP-712
  signature of the SafeTx (`eth_signTypedData_v4`), 65 bytes `r ‖ s ‖ v`.
  `ecrecover(safeTxHash, v, r, s)` must be the owner, as the Safe checks it;
  `v` 0/1 is stored as 27/28. Both kinds fill the same proposal.
- The owners are the deployed Safe's on that chain, or what its genesis
  creates there. `genesis` is optional: a Safe whose record has not landed
  yet is known by it, if it derives the address.
- `chainId` is any chain, not only a configured one: a wallet proposes
  wherever its owners have added a network. On a chain the service has no
  RPC for, the owners are always the genesis's, the address is derived with
  the known factory's own proxy code (`genesis-rules/data/proxy-creation-code.json`),
  and the nonce is not checked (`members.nonce` is `0`; the wallet reads the
  chain's own). The chain stays the last word at execution. A Safe whose
  owners changed on such a chain is judged by its genesis there.
- `201 {"status": "created", "proposal"}` · `200 {"status": "approved" | "unchanged", "proposal"}` ·
  `422 {"refusal"}` (`transaction`, `approval` with the reason, `full`) ·
  `404` for a Safe nobody can say the owners of.

`GET /api/proposals/{safe}?chainId=` — the Safe's proposals on that chain
(changed in the last 30 days, by nonce) and `members`: `deployed`, `owners`,
`threshold`, `nonce`. `GET /api/proposals/{safe}/{safeTxHash}?chainId=` — one.

A genesis an owner proved (`ownerProof`) or sent with a proposal is kept, and
`GET /api/genesis/{safe}` answers it as `"pending": true` (never cached) until
a registry holds the record.

### Caching

Responses carry `x-cache: hit | miss`. A found genesis is cached for 5 minutes
(the genesis never changes, its name can: a rename shows within that), "not
found" for 15 s (so an import shows up a moment later), per-chain status and
replay checks for 30 s, registry pages for 15 s. When the chains cannot
answer (a 5xx), an answer that succeeded within the last hour stands in,
marked `x-cache: stale`; a stale "not found" never does. The container also
lets concurrent misses of one key share a single computation.

Finished tasks are dropped 30 days after they end, proposals 30 days after
their last change (the maintenance pass). The container logs one line per
request (`target: access`); `LOG_FORMAT=json` makes every line JSON. Nothing assembled while a
chain failed to answer is cached. An import drops the Safe's cached lookups.

### Limits

Per client (the Worker: `cf-connecting-ip`; the container: the connection, or
`TRUSTED_IP_HEADER` behind a proxy), per minute: 120 reads that reach the
chains (a cache hit is free) and 10 writes (imports, proposals). Over it:
`429` with `retry-after`. The sponsor's budget is checked again in the same
step that queues an import, so concurrent imports cannot spend past it.

### Chains

Each chain's RPC endpoints are tried in turn: a failed endpoint, or one
answering with a rate limit or an overloaded node, is skipped for a minute
and the next is asked (at most three per call, 2 s to connect, 8 s each); a
revert or another answer of the request's own is final. Every request
answers within 25 s (`504` otherwise).

A sponsored write that is not mined is replaced at the same nonce with
higher fees, for as long as it stays pending: a nonce is never abandoned
while its transaction holds it, so no later write queues behind a forgotten
one. At the fee cap it waits, and the stuck-queue alert says so.

### Alerts

Every maintenance tick (cron, every minute) checks the sponsor wallet on
the home chain: low when below `MIN_SPONSOR_BALANCE_WEI`, or when it pays
for fewer than 100 records at today's fee (~500k gas each; a fee spike can
empty a balance that looks large). The message names the wallet, the chain,
the balance, the records left and the fee. It also checks every home-chain
RPC failing, the queue (a task untouched for 15 minutes, active tasks with
no alarm scheduled) and newly failed tasks. Once a day a heartbeat reports
the wallet (balance, records left), the imports, each registry's status and
the version, so a silent channel means "fine", never "alerting is down".
A condition is reported once, repeated every 6 hours while it lasts, and
reported again when it clears; failed tasks are reported each time. Delivered
over Telegram when `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` are set;
`GET /api/health` lists the active ones under `queue.alerts`.

### `GET /api/task/{id}`, `GET /api/health`

The import task: `{taskId, safe, status, txHash, nonce, attempts, error,
source, sourceChainId, provedBy, genesis, createdMs, updatedMs}`, `status`
going `queued` → `submitted` → `done` | `failed`; and the service's
configuration, sponsor address and queue counts. Health answers `503`
(`"ok": false`) when the service cannot do its job: every RPC endpoint of
the home chain failing, or (the container) the submission loop silent for 5
minutes; `rpc` lists each chain's circuit. Health's `registries` checks
each registry chain: `ok` when the address holds this service's version of the
contract (`genesis_rules::genesis::REGISTRY_CODE_HASH`), else `not deployed`,
`different code…` or `unreachable`. Lookups make the same check once per chain
and refuse to read anything else. A signed transaction is
stored before it is sent; one without a receipt is rebroadcast after a
minute and replaced at the same nonce (+25% fees) after three.

## Configuration

| Name | Kind | Default |
| --- | --- | --- |
| `REGISTRY_ADDRESS` | var or secret, required | — |
| `PRIVATE_KEY` | secret | none: read-only, imports return `recordCall` |
| `HOME_CHAIN_ID` | var | `100` (Gnosis) |
| `MAX_FEE_WEI` | var | `10000000` (0.01 gwei): above it nothing is sent, the task waits |
| `MIN_SPONSOR_BALANCE_WEI` | var | `10000000000000000` (0.01): the low-balance alert's floor |
| `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID` | secrets | none: alerts are only listed in `/api/health` |
| `CHAINS` | var | `genesis-rules/data/chains.json`: `[{"chainId", "name", "rpcs": [..], "txService", "registry"}]`; `registry: true` where the registry is deployed (Gnosis, Ethereum) |

## Run it

```shell
cargo check --target wasm32-unknown-unknown
cp .dev.vars.example .dev.vars        # then edit
npx wrangler dev
npx wrangler secret put REGISTRY_ADDRESS && npx wrangler deploy
```

Behind an HTTP proxy, worker-build's own download of `wasm-opt` can fail; point
it at a local binary instead: `WASM_OPT_BIN=/path/to/wasm-opt npx wrangler dev`.

### End to end on local forks

What was run against this crate before it was committed:

```shell
anvil --fork-url https://rpc.gnosischain.com --port 8545 &
anvil --fork-url https://base-rpc.publicnode.com --port 8546 &
(cd ../contracts && for p in 8545 8546; do
  forge script script/Deploy.s.sol --rpc-url http://127.0.0.1:$p --broadcast \
    --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80; done)
# .dev.vars: REGISTRY_ADDRESS from the script, the same anvil key, CHAINS pointing at both forks
npx wrangler dev
```

Lido's Emergency Brakes multisig on Base (3/5, `0x0F9A0e7071B7B21bc7a8514DA2cd251bc1FF0725`)
was imported from Base's transaction service, recorded on the Gnosis fork by
the sponsor, judged `ready` for Gnosis (factory, singleton and handler all
Safe 1.3.0 eip155), and deployed there from the API's `deployCall` alone at
the same address with threshold 3. A swapped owner on Gnosis then showed as
drift there and not on Base; a Safe{Wallet} counterfactual config was
imported and recorded although the Safe exists on no chain; a non-Safe
address was refused.

Then both shells together, on fresh forks with separate sponsor keys: each
imported one Lido multisig (one broadcast each), the six lookups for the two
Safes (genesis, chains, replay) came back identical from both, repeated
lookups were served from cache, the docker shell answered `recorded` for the
Safe the Worker had written, and with the balance floor set above the
sponsor's balance both raised `lowBalance`.

## Not yet

- An index of `GenesisRecorded` events (owner → Safes): left out on purpose;
  it costs a scan of every chain for a convenience the registry does not need.
