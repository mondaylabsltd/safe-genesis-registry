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

### `GET /api/genesis/{safe}`

The Safe's genesis from whichever chains hold it (home chain preferred).
`verified` re-derives the address from the record and the factory's proxy
code on the chain it was read from.

```json
{
  "safe": "0x0f9a…0725", "found": true,
  "recordedOn": [100], "readFrom": 100, "verified": true,
  "genesis": {"factory": "0x…", "singleton": "0x…", "saltNonce": "0x…", "initializer": "0x…"},
  "proxyCreationCodeHash": "0x…", "recorder": "0x…", "recordedAt": 48965199,
  "setup": {"owners": ["0x…"], "threshold": "0x3", "to": "0x…", "data": "0x", "fallbackHandler": "0x…", …},
  "setupDefects": [],
  "deployCall": {"to": "<factory>", "data": "<createProxyWithNonce calldata>"}
}
```

`404` with `"found": false` when no chain holds it; `unreachable` lists chains
that did not answer.

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
{"chainId": 8453, "safe": "0x…"}
{"chainId": 8453, "safe": "0x…", "safeWalletProps": {…}}   // a Safe deployed nowhere yet
```

Resolves the Safe's genesis on `chainId` — from the registry on another chain,
from Safe{Wallet}'s counterfactual config (`undeployedSafes[chain][safe].props`
in a Safe{Wallet} export), or from the chain's Safe Transaction Service and,
when the service has no parameters, the creating transaction's calldata. Only
parameters that reproduce the Safe's address are accepted.

- `200 {"status": "recorded"}` — the home chain already holds it.
- `202 {"taskId", "status": "queued"}` — verified and queued; the service writes
  it to the home chain at its own expense. Poll `GET /api/task/{taskId}`.
  Sponsored only for a Safe that already exists on some chain, within a budget
  (20 per minute, 500 per day).
- `200 {"status": "resolved", "recordCall", "reason"}` — not sponsored (no
  sponsor key, a Safe that exists on no chain yet, or the budget spent): the
  verified `record(...)` call, for the caller to send.
- `422 {"refusal"}` — nothing reproduced the address (`unresolved`: `notFound`,
  `mismatch` with the address the evidence gives instead, or `noCreate2` —
  `createProxy`, as Wintermute's Safe was, or a chain-specific salt), or the
  registry would reject the parameters (`unrecordable`).

### Caching

Responses carry `x-cache: hit | miss`. A found genesis is cached for an hour
(a record never changes), "not found" for 15 s (so an import shows up a moment
later), per-chain status and replay checks for 30 s. Nothing assembled while a
chain failed to answer is cached. An import drops the Safe's cached lookups.

### Alerts

Every maintenance tick (cron, every 5 minutes) checks the sponsor balance on
the home chain against `MIN_SPONSOR_BALANCE_WEI`, the queue (a task untouched
for 15 minutes, active tasks with no alarm scheduled) and newly failed tasks.
A condition is reported once, repeated every 6 hours while it lasts, and
reported again when it clears; failed tasks are reported each time. Delivered
over Telegram when `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` are set;
`GET /api/health` lists the active ones under `queue.alerts`.

### `GET /api/task/{id}`, `GET /api/health`

The import task (`queued` → `submitted` → `done` | `failed`, with `txHash`,
the stored `broadcast`, `attempts`, `errors`, `error`), and the service's
configuration, sponsor address and queue counts. A signed transaction is
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
| `CHAINS` | var | `genesis-rules/data/chains.json`: `[{"chainId", "name", "rpcs": [..], "txService"}]` |

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
