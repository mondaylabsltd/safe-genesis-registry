"""Pull every getvela.app wallet group from the public p256-index API."""
import json, sys, urllib.request, concurrent.futures as cf

BASE = "https://p256-index-v2.getvela.app/api/query?unitId="

def fetch(uid):
    try:
        req = urllib.request.Request(BASE + str(uid), headers={"User-Agent": "safe-genesis-registry-verify/1"})
        with urllib.request.urlopen(req, timeout=30) as r:
            d = json.load(r)
    except Exception as e:
        return {"unitId": uid, "error": str(e)}
    unit = d.get("unit") or {}
    if unit.get("rpId") != "getvela.app":
        return None
    try:
        meta = json.loads(bytes.fromhex(unit["metadata"]).decode())
    except Exception as e:
        return {"unitId": uid, "error": "metadata: " + str(e)}
    members = [m["publicKey"] for m in d["members"]["items"]]
    if len(members) != d["members"]["total"]:
        return {"unitId": uid, "error": "member page truncated"}
    return {"unitId": uid, "address": meta.get("address"), "walletVersion": meta.get("wallet_version"),
            "members": members}

with cf.ThreadPoolExecutor(4) as ex:
    rows = [r for r in ex.map(fetch, range(0, 260)) if r]
json.dump(rows, open(sys.argv[1], "w"), indent=1)
ok = [r for r in rows if "error" not in r]
print("groups:", len(ok), "errors:", len(rows) - len(ok))
from collections import Counter
print("members per group:", Counter(len(r["members"]) for r in ok))
print("wallet versions:", Counter(r["walletVersion"] for r in ok))
