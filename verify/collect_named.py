"""Resolve named Safe addresses to their genesis and check it.

Input: JSON list of {"chainId", "address", "label"?, "source"?} for deployed
Safes, and optionally {"chainId", "address", "undeployed": {...props}} for
Safe{Wallet}'s counterfactual Safes (its exported `undeployedSafes`).

Deployed Safes: the creation parameters come from the Safe Transaction
Service (`/safes/{address}/creation/`) of that chain. Ground truth is the
address itself and its code on chain: Safe's formula has to map the
parameters back to it.
"""
import json, sys, time, urllib.request, concurrent.futures as cf
from collections import Counter
from eth_hash.auto import keccak
from eth_abi import encode

UA = {"User-Agent": "sgr-verify/1", "Content-Type": "application/json"}
PUBLICNODE = {1: "ethereum", 10: "optimism", 56: "bsc", 100: "gnosis", 137: "polygon-bor", 8453: "base",
              42161: "arbitrum-one", 5000: "mantle", 534352: "scroll", 59144: "linea", 130: "unichain",
              43114: "avalanche-c-chain", 146: "sonic", 42220: "celo", 81457: "blast"}


def get(url, retries=5):
    for i in range(retries):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if i == retries - 1:
                raise
            time.sleep(2 * (i + 1))
        except Exception:
            if i == retries - 1:
                raise
            time.sleep(2 * (i + 1))


chains = {str(c["chainId"]): c for c in get("https://safe-config.safe.global/api/v1/chains/?limit=200")["results"]}


OVERRIDE = {1: "https://eth.drpc.org"}  # publicnode's Ethereum endpoint times out on old transactions


def rpc_url(chain):
    if chain in OVERRIDE:
        return OVERRIDE[chain]
    if chain in PUBLICNODE:
        return f"https://{PUBLICNODE[chain]}{'' if PUBLICNODE[chain].endswith('-rpc') else '-rpc'}.publicnode.com"
    c = chains.get(str(chain))
    return (c.get("publicRpcUri") or {}).get("value") if c else None


def rpc(chain, method, params, retries=5):
    url = rpc_url(chain)
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for i in range(retries):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, body, UA), timeout=60) as r:
                d = json.load(r)
            if "error" in d:
                raise RuntimeError(d["error"])
            return d["result"]
        except Exception:
            if i == retries - 1:
                raise
            time.sleep(2 * (i + 1))


_codes = {}


def proxy_code(chain, factory):
    key = (chain, factory.lower())
    if key not in _codes:
        ret = bytes.fromhex(rpc(chain, "eth_call", [{"to": factory, "data": "0x53e5d935"}, "latest"])[2:])
        off, ln = int.from_bytes(ret[:32], "big"), int.from_bytes(ret[32:64], "big")
        _codes[key] = ret[off + 32:off + 32 + ln]
    return _codes[key]


def safe_address(factory, code, singleton, initializer, salt_nonce):
    salt = keccak(keccak(initializer) + salt_nonce.to_bytes(32, "big"))
    ich = keccak(code + int(singleton, 16).to_bytes(32, "big"))
    return "0x" + keccak(b"\xff" + bytes.fromhex(factory[2:]) + salt + ich)[12:].hex()


def checksum(addr):
    a = addr.lower()[2:]
    h = keccak(a.encode()).hex()
    return "0x" + "".join(ch.upper() if int(h[i], 16) >= 8 else ch for i, ch in enumerate(a))


def setup_from_props(cfg):
    """Safe{Wallet}'s safeAccountConfig -> the setup() initializer it deploys with."""
    z = "0x" + "00" * 20
    args = encode(["address[]", "uint256", "address", "bytes", "address", "address", "uint256", "address"],
                  [cfg["owners"], int(cfg["threshold"]), cfg.get("to") or z, bytes.fromhex((cfg.get("data") or "0x")[2:]),
                   cfg.get("fallbackHandler") or z, cfg.get("paymentToken") or z, int(cfg.get("payment") or 0),
                   cfg.get("paymentReceiver") or z])
    return bytes.fromhex("b63e800d") + args


sel = lambda sig: keccak(sig.encode())[:4]
SCAN = ((sel("createProxyWithNonce(address,bytes,uint256)"), False), (sel("createProxyWithNonceL2(address,bytes,uint256)"), False),
        (sel("createProxyWithCallback(address,bytes,uint256,address)"), True))
LEGACY = {sel("createProxy(address,bytes)"): "createProxy (CREATE: the address follows the factory's nonce, not the setup)"}


def word(b, i):
    return int.from_bytes(b[i:i + 32], "big") if i + 32 <= len(b) else None


def from_calldata(chain, txh, factory, code, addr):
    """Recover (singleton, initializer, saltNonce) from the creating tx's calldata."""
    data = bytes.fromhex(rpc(chain, "eth_getTransactionByHash", [txh])["input"][2:])
    for s_, cb in SCAN:
        i = data.find(s_)
        while i != -1:
            p = i + 4
            w0, w1, w2 = word(data, p), word(data, p + 32), word(data, p + 64)
            if None not in (w0, w1, w2) and not w0 >> 160 and w1 < len(data):
                ln = word(data, p + w1)
                if ln and p + w1 + 32 + ln <= len(data):
                    init = data[p + w1 + 32:p + w1 + 32 + ln]
                    nonce = w2
                    if cb:
                        c = word(data, p + 96)
                        nonce = int.from_bytes(keccak(w2.to_bytes(32, "big") + c.to_bytes(32, "big")[12:]), "big") if c is not None else None
                    singleton = "0x%040x" % w0
                    if nonce is not None and safe_address(factory, code, singleton, init, nonce) == addr:
                        return singleton, init, nonce
            i = data.find(s_, i + 1)
    for s_, why in LEGACY.items():
        if s_ in data:
            return why
    return None


def resolve(item):
    chain, addr = int(item["chainId"]), item["address"].lower()
    base = {"chainId": chain, "proxy": addr, "label": item.get("label", ""), "source": item.get("source", "")}
    if "undeployed" in item:
        p = item["undeployed"]
        factory, singleton = p["factoryAddress"].lower(), p["masterCopy"].lower()
        init, nonce = setup_from_props(p["safeAccountConfig"]), int(p["saltNonce"])
        origin = "safe{wallet} undeployedSafes"
    else:
        c = chains.get(str(chain))
        if not c:
            return {**base, "status": "chain not served by Safe"}
        try:
            cr = get(c["transactionService"].rstrip("/") + f"/api/v1/safes/{checksum(addr)}/creation/")
        except Exception as e:
            return {**base, "status": f"transaction service error: {e}"}
        if not cr:
            return {**base, "status": "not a Safe known to the transaction service"}
        if not cr.get("factoryAddress"):
            return {**base, "status": "transaction service has no creation record"}
        factory = cr["factoryAddress"].lower()
        if cr.get("saltNonce") is not None and cr.get("setupData") not in (None, "0x"):
            singleton = cr["masterCopy"].lower()
            init, nonce = bytes.fromhex(cr["setupData"][2:]), int(cr["saltNonce"])
            origin = "transaction service creation " + str(cr.get("transactionHash"))
        else:
            # The service knows the creating tx but not the parameters.
            try:
                code = proxy_code(chain, factory)
                got = from_calldata(chain, cr["transactionHash"], factory, code, addr)
            except Exception as e:
                got = None
            if isinstance(got, str):
                return {**base, "status": "no CREATE2 genesis: " + got, "factory": factory, "txHash": cr["transactionHash"]}
            if not got:
                return {**base, "status": "parameters not in the creating tx calldata", "factory": factory,
                        "txHash": cr["transactionHash"], "pending": True}
            singleton, init, nonce = got
            origin = "creating tx calldata " + cr["transactionHash"]
    try:
        code = proxy_code(chain, factory)
    except Exception as e:
        return {**base, "status": f"no RPC / factory read failed: {e}"}
    computed = safe_address(factory, code, singleton, init, nonce)
    deployed = rpc(chain, "eth_getCode", [addr, "latest"]) not in ("0x", "0x0")
    row = {**base, "factory": factory, "singleton": singleton, "initializer": "0x" + init.hex(),
           "saltNonce": hex(nonce), "proxyCode": "0x" + code.hex(), "deployed": deployed, "origin": origin}
    if computed != addr:
        return {**row, "status": f"MISMATCH: formula gives {computed}"}
    return {**row, "status": "ok"}


items = json.load(open(sys.argv[1]))
with cf.ThreadPoolExecutor(4) as ex:
    out = list(ex.map(resolve, items))
by_addr = {}
for r in out:
    if r["status"] == "ok":
        by_addr.setdefault(r["proxy"], r)
for k, r in enumerate(out):
    if r.get("pending") and r["proxy"] in by_addr:
        g = by_addr[r["proxy"]]
        try:
            code = proxy_code(r["chainId"], g["factory"])
            computed = safe_address(g["factory"], code, g["singleton"], bytes.fromhex(g["initializer"][2:]), int(g["saltNonce"], 16))
            deployed = rpc(r["chainId"], "eth_getCode", [r["proxy"], "latest"]) not in ("0x", "0x0")
        except Exception as e:
            continue
        if computed == r["proxy"] and deployed:
            out[k] = {**g, "chainId": r["chainId"], "label": r["label"], "source": r["source"], "proxyCode": "0x" + code.hex(),
                      "deployed": True, "origin": f"genesis from chain {g['chainId']}; this chain's service has no parameters",
                      "status": "ok"}
json.dump(out, open(sys.argv[2], "w"), indent=1)
print(Counter(r["status"].split(":")[0] if r["status"].startswith(("MISMATCH", "no RPC")) else r["status"] for r in out))
for r in out:
    if r["status"] != "ok":
        print(r["chainId"], r["proxy"], r.get("label", ""), "->", r["status"][:140])
