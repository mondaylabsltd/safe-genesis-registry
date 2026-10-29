# Safe Genesis Registry

A neutral, permissionless, write-once on-chain record of **how a Safe was
created** — one record per Safe, looked up by the Safe's own address — so the
same Safe can be deployed at the same address on any other chain. No owner, no
admin, no upgrade, no fee, no external calls, no service to run. Built by Vela
Wallet; Vela is its first client, not its owner.

[中文说明](readme.zh.md)

## Repository

| Path | What |
| --- | --- |
| [`contracts/`](contracts) | The registry contract, its tests (unit, real-Safe fixtures, forks, end-to-end on a local chain and on a deployed one) and the deterministic deployment script — Foundry |
| [`genesis-rules/`](genesis-rules) | Every rule the service applies, I/O-free Rust: address derivation, genesis resolution, setup defects, the replay check, member drift, the import lifecycle |
| [`genesis-cf/`](genesis-cf) | The service on Cloudflare Workers: lookups across chains, per-chain status, replay checks, sponsored imports, cache, alerts ([API](genesis-cf/README.md)) |
| [`genesis-server/`](genesis-server) | The same service as one container (Axum, SQLite, in-memory cache): `docker compose up -d` |
| [`explorer/`](explorer) | A static page that lists the registry straight from the chain |
| [`verify/`](verify) | Scripts that build the real-Safe fixtures and the known-contracts table from public data |

## The problem

A [Safe](https://safe.global)'s address is fixed by its **creation call** —
factory, singleton, the `setup()` initializer and a salt nonce — forever. Its
owners are not: after `swapOwner` or `changeThreshold`, reading the Safe tells
you who controls it *now*, but no longer how it was made. To deploy the same
Safe on a new chain you need the original creation parameters, byte for byte,
and nothing on chain keeps them in readable form: they sit inside a historical
transaction (often nested in a 4337 bundle or a MultiSend), or in a wallet's
local storage, or in an off-chain service.

This contract keeps them, keyed by the Safe's address, readable with one
`eth_call`.

## Interface

```solidity
// Write: the registry computes the Safe's address itself; the caller never supplies it
record(address factory, bytes proxyCreationCode, address singleton, bytes initializer, uint256 saltNonce, string name)
    returns (address safe, bool created)                            // name: required, 1 to 64 bytes, not part of the address
rename(string name)                                                 // called by the Safe itself: replaces its name

// Read: by the Safe's own address
genesisOf(address safe)    returns (bool found, Genesis genesis)
deployCallOf(address safe) returns (address factory, bytes data)  // send `data` to `factory` to deploy it
isRecorded(address safe)   returns (bool)
computeAddress(factory, proxyCreationCode, singleton, initializer, saltNonce) returns (address)

// Read: in write order (index 0 = the first record on this chain)
totalRecords()             returns (uint256)
safeAt(uint256 index)      returns (address)
genesisPage(uint256 offset, uint256 limit, bool desc)               // desc = newest first
    returns (uint256 total, address[] safes, Genesis[] records)    // complete records, one eth_call
```

A `Genesis` holds `factory`, `singleton`, `saltNonce`, `initializer` (the
`setup(...)` call: original owners, threshold, fallback handler, setup hook),
`proxyCreationCodeHash`, informational `recorder` / `recordedAt`, its `index`
in this registry's write order, its `name`, and `renamedAt`.

**Names.** Whoever writes a record first chooses its name, so a name on its
own proves nothing: anyone can record a Safe whose parameters are public, and
name it. The Safe itself can replace the name at any time with `rename`:
`msg.sender` must be the Safe, so the call is the Safe's own transaction,
approved by its owners. A name is the Safe's own when `renamedAt` (the block
of its last rename, 0 if never) is not zero, or when the Safe wrote the record
itself (`recorder == safe`); otherwise it is the first writer's, and a reader
should say so. A rename changes nothing but the name. It needs the Safe
deployed on the registry's chain, and each registry deployment keeps its own
names.

A page of 100 records with ~420-byte initializers costs about 5.7M gas to
read (storage cold, as in an `eth_call`); a page's cost grows with its records' initializers, so a reader keeps
`limit` within its node's `eth_call` gas cap. `total` comes back with every
page: a reader paging newest-first compares it to tell that records arrived
in between.

## Why one Safe has exactly one genesis

The registry derives the address exactly as Safe's proxy factory does in
`createProxyWithNonce` (the same formula from v1.1.1 through v1.5.0):

```
salt         = keccak256(keccak256(initializer) ‖ uint256(saltNonce))
initCodeHash = keccak256(proxyCreationCode ‖ uint256(uint160(singleton)))
safe         = address(keccak256(0xff ‖ factory ‖ salt ‖ initCodeHash)[12:])
```

Every stored field is an input to that address. A different singleton,
initializer, salt nonce, factory or proxy code gives a different address, so
filing other parameters under an existing Safe's address takes a 160-bit
address preimage. Hence **one record per Safe, write-once**: anyone may write
it, before or after deployment, and nobody can change it, overwrite it, or
squat an existing Safe's address with something else. Writing it again returns
`created = false`.

What a record proves, and what it does not:

- **It proves** that these exact parameters, sent to `factory`, are the
  CREATE2 inputs of this address.
- **It does not prove** that the contract at `factory` is Safe's factory, or
  that a replay creates the same *wallet*. On another chain the factory, the
  singleton, the fallback handler, a setup hook or a module may be other code
  at the same address — a hostile factory can return the right proxy code and
  deploy the right address with its own setup. A replay is checked on the
  target chain (below), never trusted from the record.
- **For a Safe nobody has deployed yet**, a party that chose both parameter
  sets could make two creations share one address with a ~2^80 birthday
  search. Fund an undeployed Safe only on parameters you generated yourself.

`proxyCreationCode` is hashed, not stored (it is the same few hundred bytes
for every Safe of a factory version). A wrong one only files the record under
an address no Safe lives at.

## Scope and limits

- **Safe versions**: any version whose factory derives the salt this way.
  Factory, proxy creation code and singleton are parameters, not constants.
  If a future Safe factory changes the salt formula, that is a new registry
  contract; this one stays valid for every Safe it already holds.
- **`createProxyWithCallback`** (v1.1.1 – v1.4.1): such a Safe is the
  nonce-form Safe with `saltNonce = uint256(keccak256(saltNonce ‖ callback))`.
  Record it in that form; a replay deploys the same Safe, with the same setup,
  without calling the callback.
- **`createProxyWithNonceL2`** (v1.5.0) uses the same salt; `deployCallOf`
  returns the plain `createProxyWithNonce`, which skips the L2 event some
  indexers follow — call the L2 function with the same arguments if that matters.
- **`createChainSpecificProxyWithNonce`** is out of scope: its address differs
  on every chain, so there is nothing to carry across.
- **Genesis is not the present.** A Safe deployed from its genesis starts with
  its ORIGINAL owners. If owners changed on the source chain, the new copy
  still has the old set until it is synced through the Safe's own
  transactions — a wallet must say so before anyone sends funds there.
- **EVM CREATE2 only.** Chains that derive CREATE2 addresses differently
  (zkSync Era) are out of scope.
- **Replays through v1.0.0 – v1.3.0 factories on a chain without the
  singleton** are a hostile-party risk, not just a wallet's mistake: those
  factories do not check that the singleton exists, so anyone can deploy the
  proxy there uninitialized, and whoever calls `setup` first once the singleton
  appears owns the address. Treat such a chain as unsafe to fund for that Safe.
- `initializer` is at most 8 KiB (a full one costs about 7M gas to record,
  well under the 16.78M per-transaction cap); zero factory or singleton
  addresses and empty inputs are rejected.
- `totalRecords`, `isRecorded`, `safeAt` and `genesisPage` cover any record,
  including ones filed at addresses no Safe lives at; they do not show that an
  address is a Safe. Write order is per chain: the same Safe has a different
  `index` on each chain.
- The deterministic deployer this deploys through exists only where its
  pre-EIP-155 transaction can be included; a chain that refuses those gets the
  registry at another address. `recordedAt` is the chain's block number (on
  Arbitrum, the L1 block number).

## Using it (for wallets)

- **Where records live:** the registry is deployed on **Gnosis** and
  **Ethereum** only, not on every chain a Safe uses; a Safe on any other chain
  is deployed there from the record on one of these two (see *Expanding to a
  new chain*).
  - **Gnosis**: the service writes records at its own expense (sponsored).
  - **Ethereum**: the wallet's user writes the record and pays for it,
    typically from the Safe itself, so the record's `recorder` is the Safe and
    its name is the Safe's own.
  - **The service** reads Gnosis only (lookups, and the writes it sponsors
    there). Wallets read the Ethereum registry and every other chain
    themselves.
- **Creating a Safe:** `record` its genesis. Deploying is not needed: the
  address is fixed on every chain from that moment, and the first transaction
  on a chain can deploy it from `deployCallOf`. The service records a Safe
  deployed nowhere yet at its own expense on Gnosis when an owner's passkey asks
  (`ownerProof`, see the [API](genesis-cf/README.md)).
- **Renaming** is the user's own act, at the user's own cost; the service
  never sponsors it (it sponsors only records on Gnosis). The Safe calls
  `rename` on the registry's chain, so it must be deployed there: the wallet
  deploys it from `deployCallOf` if needed (on Gnosis a fraction of a cent),
  then sends the owners' Safe transaction `{to: registry, data: rename(name)}`
  like any other, paying its gas. The copy on Gnosis starts with the original
  owners; an owner removed on other chains still holds a vote there until
  the owners are synced, so sync before relying on its name.
- **A Safe created elsewhere** (for example by Safe{Wallet}): fetch its
  creation parameters once (the creation transaction, or a transaction
  service), then `record` them. From then on nobody needs that source again.
  If `record` returns an address other than the Safe's, the parameters were
  wrong.
- **Expanding to a new chain** (the service's `GET /api/replay` does steps 2–3
  and refuses anything it cannot vouch for):
  1. `deployCallOf(safe)` on any chain where the record exists.
  2. On the target chain, require the **runtime code hash** of the factory, the
     singleton, the fallback handler, every setup hook and every contract a
     hook calls or enables to be Safe's known code for that role, and
     `keccak256(factory.proxyCreationCode())` to equal the record's
     `proxyCreationCodeHash`. Refuse a setup hook you cannot decode completely.
  3. Owners that are contracts must be bound to their own genesis (or hold the
     same code on both chains); otherwise whoever can deploy at that address
     on the target chain holds a vote.
  4. Send `data` to `factory` (simulate first), then read the deployed Safe's
     owners, threshold, modules, fallback handler and guard and compare them
     with the initializer. Optionally `record` on that chain too.
  5. Sync the owners to the current set before showing the address as safe to
     receive on that chain.

Gas, with this repo's fixtures (3-owner setup, ~420-byte initializer): about
37k for a rename, 433k for the first write, 35k for a repeat; a full 8 KiB initializer about 7M. Storage dominates; it is cheap on
rollups and noticeable on Ethereum mainnet.

## Deployment

One contract, no constructor arguments, deployed through the deterministic
CREATE2 deployer (`0x4e59b44847b379578588920cA78FbF26c0B4956C`) with a fixed
salt, so it has the same address on every chain. Compiler settings are pinned
in `foundry.toml` (solc 0.8.28, optimizer 200, `paris`, no metadata hash), so
the address depends on code alone, not comments: v4 (a Safe renames itself,
`rename`) is `0xF412388bF908E510066d3Be5c704bd9A327f945d`.

```shell
cd contracts
forge script script/Deploy.s.sol --rpc-url $RPC                              # print the address
forge script script/Deploy.s.sol --rpc-url $RPC --broadcast --account deployer
```

Production deployments are on Gnosis (written by the service, sponsored) and
Ethereum (written by wallet users at their own cost); testnets are for testing.
The contract has no owner and no constructor arguments, so anyone deploying it
anywhere gets the same contract at the same address.

v4 deployments:

| Network | Address | Deployment |
| --- | --- | --- |
| Gnosis (100) | `0xF412388bF908E510066d3Be5c704bd9A327f945d` | tx `0x0c2d8c40d951881cc6f9179a8f2fa1bdd5cf2f34938b85f00239d7b22fc4c5eb`, block 48999181; runtime code identical to this repo's build; source verified on Sourcify |
| Ethereum (1) | `0xF412388bF908E510066d3Be5c704bd9A327f945d` | tx `0xb2c83e7e6b1d52aa8064539862851d831fded50a1b844123616f3d02aa7ec342`, block 26296633; runtime code identical to this repo's build; source verified on Sourcify and Blockscout |
| Arc Testnet (5042002) | `0xF412388bF908E510066d3Be5c704bd9A327f945d` | tx `0xd432aada7afd6e164fe394f0030695c72b7a49772d7f97a60e126eb8457705af`, block 69618452; runtime code identical to this repo's build; source verified |

v3 (a required name, records enumerable in write order; no rename, retired):

| Network | Address | Deployment |
| --- | --- | --- |
| Arc Testnet (5042002) | `0x535fE27b2b52EfbEFFfE2409890C2A00e7FF801E` | tx `0x44b2cfa3fc1fc326621c44ce062cba23b0318432a26f5264a984275dc29d277e`, block 69608861; runtime code identical to this repo's build |

v2 (a name written with the record, `recordNamed`; not enumerable, retired):

| Network | Address | Deployment |
| --- | --- | --- |
| Arc Testnet (5042002) | `0x70b35C0FC8885019ff938542CD4E431725874802` | tx `0x45f305fa8bd65836d65ee2e827bb7de1f093807ea85253dbbb6ec3f473f9cd7e`, block 69439613 |

v1 (no names, 0 records):

| Network | Address | Deployment |
| --- | --- | --- |
| Gnosis (100) | `0xdA88357C2eB85e7e0515B1122a28befd6Dc8fb89` | tx `0x2d0333a541fca54184abaca2ad37bbb25779875f8922733c0801f013be214548`, block 48968687; runtime code identical to this repo's build |
| Arc Testnet (5042002) | `0xdA88357C2eB85e7e0515B1122a28befd6Dc8fb89` | tx `0x180019e760a9727356fa16490001a099fd92282c56f534d84a2d7d9eb7415854`, block 69295041; runtime code identical to this repo's build |

Every chain gets this same address; deploy it anywhere else with `script/Deploy.s.sol` (it refuses if the deterministic deployer is missing).

## Development

```shell
cargo test                                    # genesis-rules, against the same real-Safe fixtures
cargo test -p genesis-server                  # the service over HTTP against a mock chain: 404 vs 502, pages, cache, limits, health, failover, budget
scripts/smoke.sh http://127.0.0.1:8080 100    # either running shell (or a deployment), read-only: the same checks for both
cd contracts
forge build
forge test                                    # unit tests, no network
FORK_RPC_GNOSIS=https://rpc.gnosischain.com \
FORK_RPC_BASE=https://mainnet.base.org \
forge test --mt Fork -vv                      # real Safe v1.3.0 / v1.4.1 on forks
e2e/local.sh                                  # deploy on anvil, write, read back (add FORK_RPC_GNOSIS for real Safe v1.4.1)
```

On a deployed registry (Arc Testnet holds a seeded set):

```shell
RPC=https://rpc.testnet.arc.network contracts/e2e/onchain.sh                  # read-only checks
RPC=... PRIVATE_KEY=0x... contracts/e2e/onchain.sh                            # + write, revert, replay a real Safe v1.4.1
RPC=... PRIVATE_KEY=0x... contracts/e2e/seed.sh                               # + 30 varied records (permanent)
```

`e2e/local.sh` deploys the registry as production does (deterministic
deployer, the production salt) on a fresh anvil and checks it with real
transactions: events, the raw storage layout, write-once behaviour, every
revert with its arguments, name edge cases, pagination against `safeAt`, the
`eth_call` cost of pages, and a replay from `deployCallOf`.

The fork tests create a Safe on Gnosis and record its genesis, swap an owner
there, read the genesis back by the Safe's address and deploy it on Base: same
address, original owners — for both Safe v1.3.0 and v1.4.1. `RealSafesTest`
does the same for every real Safe in `contracts/test/data/` and fails on any
revert it did not predict from the setup; after each replay it checks owners,
threshold, fallback handler and guard. The contract and the service were
reviewed adversarially (2026-10-27); the findings and what changed are in
[docs/review-2026-10-27.md](docs/review-2026-10-27.md).

## Explorer

[`explorer/index.html`](explorer/index.html) is a static page that lists the
registry straight from the chain: `genesisPage` for each page, newest or
oldest first, 20/50/100 per page, lookup by address, and per record the
decoded owners and threshold, Safe version, factory, salt nonce, recorder,
whether the Safe is deployed on that chain, and its deploy call. Names are
read as bytes and shown as text only, so a name that is not UTF-8 or that
carries markup is displayed, never interpreted.

```shell
python3 -m http.server -d explorer 8765   # then open http://127.0.0.1:8765/
# another chain or registry: ?rpc=https://…&registry=0x…&explorer=https://…
```

## License

MIT
