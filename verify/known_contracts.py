"""Generate genesis-rules/data/known-contracts.json: the contracts a Safe's
creation may legitimately depend on, with the runtime code hash each must
have on a chain before a genesis is replayed there.

- Safe's own contracts (v1.1.1, v1.3.0, v1.4.1, v1.5.0): address and codeHash
  from safe-deployments, pinned to one commit. zkSync deployments are left
  out (different CREATE2 rule, out of scope).
- Contracts outside safe-deployments that real setups call (Safe's 4337
  module, its module setup, Safe's passkey signers — what Vela wallets use):
  the code hash is READ from Gnosis and Base and kept only if both agree.

Usage: known_contracts.py out.json
"""
import json, sys, urllib.request
from eth_hash.auto import keccak

COMMIT = "b45417dd009957effb3575928941e736f8fd37d1"  # safe-deployments main, 2026-10-26
VERSIONS = ["v1.1.1", "v1.3.0", "v1.4.1", "v1.5.0"]
OBSERVE = {  # address -> name; versions per safe-modules releases Vela pins
    "0x75cf11467937ce3F2f357CE24ffc3DBF8fD5c226": "Safe4337Module v0.3.0",
    "0xa581c4A4DB7175302464fF3C06380BC3270b4037": "Safe4337Module v0.2.0",
    "0x8EcD4ec46D4D2a6B64fE960B3D64e8B94B2234eb": "SafeModuleSetup v0.2.0",
    "0x2dd68b007B46fBe91B9A7c3EDa5A7a1063cB5b47": "SafeModuleSetup v0.3.0",
    "0x94a4F6affBd8975951142c3999aEAB7ecee555c2": "SafeWebAuthnSharedSigner v0.2.1",
    "0x1d31F259eE307358a26dFb23EB365939E8641195": "SafeWebAuthnSignerFactory v0.2.1",
    "0x4E27b51350e6c2083EE19011120F50DAfEc5CA50": "SafeWebAuthnSignerSingleton v0.2.1",
}
RPCS = {100: "https://rpc.gnosischain.com", 8453: "https://base-rpc.publicnode.com"}
UA = {"User-Agent": "sgr-verify/1", "Content-Type": "application/json"}


def get(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60) as r:
        return json.load(r)


def code_hash(chain, address):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_getCode", "params": [address, "latest"]}).encode()
    with urllib.request.urlopen(urllib.request.Request(RPCS[chain], body, UA), timeout=60) as r:
        code = json.load(r)["result"]
    return None if code in ("0x", "0x0") else "0x" + keccak(bytes.fromhex(code[2:])).hex()


entries = {}
api = f"https://api.github.com/repos/safe-global/safe-deployments/contents/src/assets/{{}}?ref={COMMIT}"
raw = f"https://raw.githubusercontent.com/safe-global/safe-deployments/{COMMIT}/src/assets/{{}}/{{}}"
for v in VERSIONS:
    for f in get(api.format(v)):
        asset = get(raw.format(v, f["name"]))
        for kind, dep in (asset.get("deployments") or {}).items():
            if kind == "zksync" or not dep.get("codeHash"):
                continue
            key = (dep["address"].lower(), dep["codeHash"].lower())
            entries[key] = {"address": dep["address"].lower(), "codeHash": dep["codeHash"].lower(),
                            "name": f'{asset["contractName"]} {asset["version"]} ({kind})',
                            "source": f"safe-deployments@{COMMIT[:8]}"}
        if not asset.get("deployments"):
            # Older assets carry a single defaultAddress and no code hash: read it.
            addr = asset["defaultAddress"]
            hashes = {c: code_hash(c, addr) for c in RPCS}
            print("no codeHash in safe-deployments:", v, f["name"], addr, hashes, file=sys.stderr)
for addr, name in OBSERVE.items():
    hashes = {c: code_hash(c, addr) for c in RPCS}
    assert len(set(hashes.values())) == 1 and None not in hashes.values(), (name, hashes)
    h = hashes[100]
    entries[(addr.lower(), h)] = {"address": addr.lower(), "codeHash": h, "name": name,
                                 "source": "observed identical on chains " + ", ".join(map(str, RPCS))}

out = sorted(entries.values(), key=lambda e: (e["name"], e["address"]))
json.dump({"safeDeploymentsCommit": COMMIT, "contracts": out}, open(sys.argv[1], "w"), indent=1)
print(len(out), "known contracts")
