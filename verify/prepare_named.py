"""Turn collect_named.py output into a forge fixture (same shape as
real-safes.json). One row per address; every chain's independently resolved
genesis for that address must be identical.

Usage: prepare_named.py named_out.json out.json [--no-labels]
"""
import json, sys, time, urllib.request, concurrent.futures as cf
from collections import defaultdict

RPC = {100: "https://rpc.gnosischain.com", 8453: "https://base-rpc.publicnode.com"}


def rpc(url, method, params, retries=6):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for i in range(retries):
        try:
            req = urllib.request.Request(url, body, {"Content-Type": "application/json", "User-Agent": "sgr-verify/1"})
            with urllib.request.urlopen(req, timeout=60) as r:
                d = json.load(r)
            if "error" in d:
                raise RuntimeError(d["error"])
            return d["result"]
        except Exception:
            if i == retries - 1:
                raise
            time.sleep(1.5 * (i + 1))


rows = [r for r in json.load(open(sys.argv[1])) if r["status"] == "ok"]
no_labels = "--no-labels" in sys.argv
by_addr = defaultdict(list)
for r in rows:
    by_addr[r["proxy"]].append(r)

out_rows, multi_chain = [], 0
for addr, rs in by_addr.items():
    g = {(r["factory"], r["singleton"], r["initializer"], r["saltNonce"]) for r in rs}
    assert len(g) == 1, f"{addr}: different genesis on different chains: {[(r['chainId'], r['origin']) for r in rs]}"
    if len({r["chainId"] for r in rs}) > 1:
        multi_chain += 1
    deployed_on = sorted({r["chainId"] for r in rs if r["deployed"]})
    first = rs[0]
    out_rows.append({**first, "deployedOn": deployed_on, "chains": sorted({r["chainId"] for r in rs})})


def has_code(args):
    chain, addr = args
    return rpc(RPC[chain], "eth_getCode", [addr, "latest"]) not in ("0x", "0x0")


with cf.ThreadPoolExecutor(6) as ex:
    for chain in RPC:
        for r, f in zip(out_rows, ex.map(has_code, [(chain, r["proxy"]) for r in out_rows])):
            r[f"on{chain}"] = f

codes = sorted({r["proxyCode"] for r in out_rows})
fixture = {
    "proxyCodes": codes,
    "codeIndex": [codes.index(r["proxyCode"]) for r in out_rows],
    "proxy": [r["proxy"] for r in out_rows],
    "factory": [r["factory"] for r in out_rows],
    "singleton": [r["singleton"] for r in out_rows],
    "initializer": [r["initializer"] for r in out_rows],
    "saltNonce": [str(int(r["saltNonce"], 16)) for r in out_rows],
    # 0 = replay on every target chain where the Safe does not exist yet
    "sourceChain": [0 for _ in out_rows],
    "onGnosis": [r["on100"] for r in out_rows],
    "onBase": [r["on8453"] for r in out_rows],
    "label": [("deployed on " + ",".join(map(str, r["deployedOn"])) if r["deployedOn"] else "undeployed (Safe{Wallet} config)")
              if no_labels else f'{r["label"]} | {r["source"]}' for r in out_rows],
}
json.dump(fixture, open(sys.argv[2], "w"))
print("rows", len(out_rows), "addresses seen on several chains with identical genesis:", multi_chain)
