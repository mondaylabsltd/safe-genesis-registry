"""Merge sampled Safes and Vela wallets into the forge fixture, marking for
each row whether it already exists on Gnosis / Base.

Usage: prepare.py safes_raw.json vela_derived.json out.json
"""
import json, sys, time, urllib.request, concurrent.futures as cf

RPC = {100: "https://rpc.gnosischain.com", 8453: "https://base-rpc.publicnode.com"}
VELA_FACTORY = "0x4e1dcf7ad4e460cfd30791ccc4f9c8a4f820ec67"


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


safes = json.load(open(sys.argv[1]))
vela = json.load(open(sys.argv[2]))
codes = dict(safes["proxyCodes"])  # "chain:factory" -> code

rows = []
for r in safes["rows"]:
    rows.append({**r, "source": "safe", "codeKey": f'{r["chainId"]}:{r["factory"]}'})
for v in vela:
    rows.append({"chainId": 0, "version": "vela/1.4.1", "factory": v["factory"].lower(), "proxy": v["address"].lower(),
                 "singleton": v["singleton"].lower(), "initializer": v["initializer"],
                 "saltNonce": hex(int(v["saltNonce"], 16)), "method": "vela-core compute_safe_address_multi",
                 "keyCount": v["keyCount"], "source": "vela", "codeKey": f"100:{VELA_FACTORY}"})


# One row per address: the index may hold the same wallet in several groups
# (the same key registered more than once), and a Safe can appear on both chains.
seen, unique = set(), []
for r in rows:
    if r["proxy"] not in seen:
        seen.add(r["proxy"])
        unique.append(r)
print("duplicate addresses dropped:", len(rows) - len(unique))
rows = unique


def has_code(args):
    chain, addr = args
    return rpc(RPC[chain], "eth_getCode", [addr, "latest"]) not in ("0x", "0x0")


with cf.ThreadPoolExecutor(6) as ex:
    for chain in RPC:
        flags = list(ex.map(has_code, [(chain, r["proxy"]) for r in rows]))
        for r, f in zip(rows, flags):
            r[f"onChain{chain}"] = f

code_keys = sorted({r["codeKey"] for r in rows})
out = {
    "proxyCodes": [codes[k] for k in code_keys],
    "proxyCodeKeys": code_keys,
    "codeIndex": [code_keys.index(r["codeKey"]) for r in rows],
    "proxy": [r["proxy"] for r in rows],
    "factory": [r["factory"] for r in rows],
    "singleton": [r["singleton"] for r in rows],
    "initializer": [r["initializer"] for r in rows],
    "saltNonce": [str(int(r["saltNonce"], 16)) for r in rows],
    "sourceChain": [r["chainId"] for r in rows],
    "onGnosis": [r["onChain100"] for r in rows],
    "onBase": [r["onChain8453"] for r in rows],
    "label": [f'{r["source"]} {r["version"]} chain {r["chainId"]} {r["method"]}' for r in rows],
    "provenance": [{k: r[k] for k in ("source", "version", "chainId", "method", "txHash", "keyCount") if k in r} for r in rows],
}
json.dump(out, open(sys.argv[3], "w"))
from collections import Counter
print("rows", len(rows))
print("vela deployed on gnosis/base:", Counter((r["onChain100"], r["onChain8453"]) for r in rows if r["source"] == "vela"))
print("safe rows existing on the OTHER chain:",
      sum(1 for r in rows if r["source"] == "safe" and r[f'onChain{8453 if r["chainId"] == 100 else 100}']))
