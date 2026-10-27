"""Sample real Safes from ProxyCreation events and recover their creation
parameters from the creating transaction's calldata.

Ground truth is the factory's own event: "factory F deployed proxy P". The
parameters are accepted only if Safe's formula maps them back to P; the
registry contract is then tested against the same rows in forge.
"""
import json, sys, time, urllib.request
from collections import Counter, defaultdict
from eth_hash.auto import keccak

CHAINS = {
    100: ("https://rpc.gnosischain.com", 20000),
    8453: ("https://base-rpc.publicnode.com", 5000),
}
FACTORIES = {
    "0xa6b71e26c5e0845f74c812102ca7114b6a896ab2": "1.3.0",
    "0xc22834581ebc8527d974f8a1c97e1bea4ef910bc": "1.3.0-eip155",
    "0x4e1dcf7ad4e460cfd30791ccc4f9c8a4f820ec67": "1.4.1",
    "0x14f2982d601c9458f93bd70b218933a6f8165e7b": "1.5.0",
}
TARGET = int(sys.argv[2]) if len(sys.argv) > 2 else 40
MULTI_TARGET = int(sys.argv[3]) if len(sys.argv) > 3 else 25
MAX_WINDOWS = int(sys.argv[4]) if len(sys.argv) > 4 else 60
from eth_abi import decode as abi_decode


def threshold_of(init):
    """The threshold a Safe `setup` initializer sets, or None."""
    if init[:4].hex() != "b63e800d":
        return None
    try:
        args = abi_decode(["address[]", "uint256", "address", "bytes", "address", "address", "uint256", "address"], init[4:])
        return args[1]
    except Exception:
        return None

sel = lambda sig: keccak(sig.encode())[:4]
SEL_NONCE = sel("createProxyWithNonce(address,bytes,uint256)")
SEL_NONCE_L2 = sel("createProxyWithNonceL2(address,bytes,uint256)")
SEL_CALLBACK = sel("createProxyWithCallback(address,bytes,uint256,address)")
SEL_CHAIN = sel("createChainSpecificProxyWithNonce(address,bytes,uint256)")
SEL_CHAIN_L2 = sel("createChainSpecificProxyWithNonceL2(address,bytes,uint256)")
TOPIC_CREATION = "0x" + keccak(b"ProxyCreation(address,address)").hex()
TOPIC_CREATION_L2 = "0x" + keccak(b"ProxyCreationL2(address,address,bytes,uint256)").hex()


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
        except Exception as e:
            if i == retries - 1:
                raise
            time.sleep(1.5 * (i + 1))


def word(b, i):
    return int.from_bytes(b[i:i + 32], "big") if i + 32 <= len(b) else None


def create2(factory, salt, init_code_hash):
    return "0x" + keccak(b"\xff" + bytes.fromhex(factory[2:]) + salt + init_code_hash)[12:].hex()


def safe_address(factory, proxy_code, singleton, initializer, salt_nonce):
    salt = keccak(keccak(initializer) + salt_nonce.to_bytes(32, "big"))
    ich = keccak(proxy_code + int(singleton, 16).to_bytes(32, "big"))
    return create2(factory, salt, ich)


def decode_at(data, p, with_callback):
    """ABI-decode (address, bytes, uint256[, address]) starting at p."""
    w0, w1, w2 = word(data, p), word(data, p + 32), word(data, p + 64)
    if None in (w0, w1, w2) or w0 >> 160 or w1 > len(data):
        return None
    cb = None
    if with_callback:
        cb = word(data, p + 96)
        if cb is None or cb >> 160:
            return None
    ln = word(data, p + w1)
    if ln is None or p + w1 + 32 + ln > len(data) or ln == 0:
        return None
    init = data[p + w1 + 32:p + w1 + 32 + ln]
    return "0x%040x" % w0, init, w2, cb


def candidates(data):
    for s, kind in ((SEL_NONCE, "createProxyWithNonce"), (SEL_NONCE_L2, "createProxyWithNonceL2"),
                    (SEL_CALLBACK, "createProxyWithCallback"), (SEL_CHAIN, "chainSpecific"),
                    (SEL_CHAIN_L2, "chainSpecific")):
        i = data.find(s)
        while i != -1:
            d = decode_at(data, i + 4, kind == "createProxyWithCallback")
            if d:
                yield kind, d
            i = data.find(s, i + 1)


def proxy_code(url, factory):
    ret = bytes.fromhex(rpc(url, "eth_call", [{"to": factory, "data": "0x" + sel("proxyCreationCode()").hex()}, "latest"])[2:])
    off, ln = word(ret, 0), word(ret, 32)
    return ret[off + 32:off + 32 + ln]


rows, stats = [], defaultdict(Counter)
creators = defaultdict(Counter)
codes = {}
for chain, (url, window) in CHAINS.items():
    head = int(rpc(url, "eth_blockNumber", []), 16)
    for factory, version in FACTORIES.items():
        if rpc(url, "eth_getCode", [factory, "latest"]) in ("0x", "0x0"):
            stats[(chain, version)]["factory missing"] += 1
            continue
        code = proxy_code(url, factory)
        codes[(chain, factory)] = code
        got, multi, seen, to = 0, 0, set(), head
        for _ in range(MAX_WINDOWS):
            if got >= TARGET and multi >= MULTI_TARGET:
                break
            frm = max(0, to - window + 1)
            logs = rpc(url, "eth_getLogs", [{"address": factory, "fromBlock": hex(frm), "toBlock": hex(to),
                                             "topics": [[TOPIC_CREATION, TOPIC_CREATION_L2]]}])
            to = frm - 1
            by_tx = defaultdict(set)
            l2 = {}
            for lg in logs:
                data = bytes.fromhex(lg["data"][2:])
                proxy = ("0x" + lg["topics"][1][26:]) if len(lg["topics"]) > 1 else "0x" + data[12:32].hex()
                if lg["topics"][0] == TOPIC_CREATION_L2:
                    l2[proxy] = data
                by_tx[lg["transactionHash"]].add(proxy)
            for txh, proxies in by_tx.items():
                if got >= TARGET and multi >= MULTI_TARGET:
                    break
                proxies -= seen
                if not proxies:
                    continue
                tx = rpc(url, "eth_getTransactionByHash", [txh])
                data = bytes.fromhex(tx["input"][2:])
                for proxy in proxies:
                    seen.add(proxy)
                    stats[(chain, version)]["proxies seen"] += 1
                    hit = None
                    if proxy in l2:  # v1.5.0 L2 event carries the parameters itself
                        d = decode_at(l2[proxy], 0, False)
                        # event data is (singleton, bytes, uint256) with the proxy indexed
                        if d and safe_address(factory, code, *d[:3]) == proxy:
                            hit = ("event:ProxyCreationL2", d[0], d[1], d[2])
                    if not hit:
                        kinds = set()
                        for kind, (singleton, init, nonce, cb) in candidates(data):
                            kinds.add(kind)
                            if kind == "createProxyWithCallback":
                                nonce = int.from_bytes(keccak(nonce.to_bytes(32, "big") + bytes.fromhex("%040x" % cb)), "big")
                            if kind != "chainSpecific" and safe_address(factory, code, singleton, init, nonce) == proxy:
                                hit = (kind, singleton, init, nonce)
                                break
                        if not hit:
                            reason = "chain-specific salt (out of scope)" if kinds == {"chainSpecific"} else \
                                "selector present, no candidate matched" if kinds else "creation params not in tx calldata"
                            stats[(chain, version)][reason] += 1
                            if reason.startswith("creation params"):
                                creators[(chain, version)][tx.get("to")] += 1
                            continue
                    kind, singleton, init, nonce = hit
                    t = threshold_of(init)
                    is_multi = t is not None and t > 1
                    if got >= TARGET and not (is_multi and multi < MULTI_TARGET):
                        continue
                    multi += is_multi
                    stats[(chain, version)]["matched: " + kind] += 1
                    stats[(chain, version)]["kept multisig (threshold>1)"] += is_multi
                    rows.append({"chainId": chain, "version": version, "factory": factory, "proxy": proxy,
                                 "singleton": singleton, "initializer": "0x" + init.hex(), "saltNonce": hex(nonce),
                                 "method": kind, "txHash": txh})
                    got += 1
        print(f"chain {chain} {version}: kept {got} (multisig {multi})", file=sys.stderr)

json.dump({"rows": rows, "proxyCodes": {f"{c}:{f}": "0x" + v.hex() for (c, f), v in codes.items()}},
          open(sys.argv[1], "w"), indent=1)
for k, v in sorted(stats.items()):
    print(k, dict(v))
for k, v in sorted(creators.items()):
    print("creators without calldata params", k, v.most_common(3))
