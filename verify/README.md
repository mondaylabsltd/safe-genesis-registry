# Verifying the registry against real Safes

`contracts/test/data/real-safes.json` is built from public chain data by the scripts
here, and `contracts/test/RealSafes.t.sol` checks the registry against it. The ground
truth is never the registry's own formula:

- **Safes** come from the factories' `ProxyCreation` events on Gnosis and Base
  (Safe v1.3.0 canonical and eip155, v1.4.1, v1.5.0) — "factory F deployed
  proxy P". The creation parameters are recovered from the creating
  transaction's calldata (also when nested in a 4337 bundle or a MultiSend)
  and kept only if Safe's formula maps them back to P.
- **Vela wallets** come from the public p256-index (`rpId = getvela.app`),
  each derived with vela-core's own `compute_safe_address_multi` and kept only
  if it reproduces the address the wallet registered.
- **Named Safes** (`verify/known-safes.in.json`, each with its source URL:
  Lido's published multisigs, the Bybit cold wallet, Wintermute's Safe, the
  ENS Security Council, SafeDAO's treasuries) and, locally, a Safe{Wallet}
  export: `collect_named.py` takes each address's creation parameters from
  the chain's Safe Transaction Service, falls back to the creating
  transaction's calldata, then to a genesis already verified for the same
  address on another chain; Safe{Wallet}'s `undeployedSafes` configs are
  used as they are. Every row must reproduce its own address.
- The fork tests then hand **only `genesisOf`'s fields** to the real factory on
  the other chain and require the same address, code, owners and threshold.

## Rebuild the fixture

```shell
pip install eth-hash[pycryptodome] eth-abi
python3 verify/collect_safes.py safes_raw.json 40 25 150   # per factory: 40 Safes, at least 25 with threshold > 1, up to 150 log windows
python3 verify/collect_vela.py vela_units.json
python3 verify/known_contracts.py genesis-rules/data/known-contracts.json   # the replay check's code-hash table
(cd verify/vela-derive && cargo run --release -- ../../vela_units.json) > vela_derived.json   # needs a vela-wallet checkout, see Cargo.toml
python3 verify/prepare.py safes_raw.json vela_derived.json contracts/test/data/real-safes.json

python3 verify/collect_named.py verify/known-safes.in.json known_out.json
python3 verify/prepare_named.py known_out.json contracts/test/data/known-safes.json
# a Safe{Wallet} export (Settings -> Data -> Export), kept out of git:
python3 verify/collect_named.py user_in.json user_out.json   # user_in.json: see collect_named.py's docstring
python3 verify/prepare_named.py user_out.json contracts/test/data/user-safes.json --no-labels
```

## Run

```shell
cd contracts
forge test --mc RealSafesTest                                   # every row filed under its real address
FORK_RPC_GNOSIS=https://rpc.gnosischain.com \
FORK_RPC_BASE=https://base-rpc.publicnode.com \
forge test --mc RealSafesTest --mt Fork -vv                     # deploy every row on the other chain from genesisOf
```

Safes whose creation parameters are not in any transaction's calldata (a
contract assembled them on chain) are counted by `collect_safes.py` and left
out: their genesis has to come from a trace, and the registry checks it the
same way once someone records it.

The same rows are also checked by `genesis-rules` (`cargo test` at the repo
root), so the address rule has three independent implementations agreeing on
real data: the Solidity registry, the Python collectors, and the Rust rules.
