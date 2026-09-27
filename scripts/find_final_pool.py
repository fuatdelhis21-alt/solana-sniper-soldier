"""Holder kapısından geçen tokenlerin CLMM havuzlarını bulur ve tüm kapıları test eder."""
import base64
import json
import time
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"
CLMM = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"
WSOL = "So11111111111111111111111111111111111111112"
MIN_LIQ = 1_000_000_000_000
MAX_SINGLE, MAX_TOP20 = 30.0, 70.0

OFF_MINT0, OFF_MINT1 = 73, 105
OFF_VAULT0, OFF_VAULT1 = 137, 169
OFF_SQRT = 253


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=45).read())["result"]


def b58e(b):
    al = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = al[r] + out
    return "1" * (len(b) - len(b.lstrip(b"\x00"))) + (out or "")


def pool_state(pid):
    v = rpc("getAccountInfo", [pid, {"encoding": "base64"}])["value"]
    if v is None:
        return None
    d = base64.b64decode(v["data"][0])
    if len(d) < 269:
        return None
    return {
        "mint0": b58e(d[OFF_MINT0 : OFF_MINT0 + 32]),
        "mint1": b58e(d[OFF_MINT1 : OFF_MINT1 + 32]),
        "vault0": b58e(d[OFF_VAULT0 : OFF_VAULT0 + 32]),
        "vault1": b58e(d[OFF_VAULT1 : OFF_VAULT1 + 32]),
        "sqrt": int.from_bytes(d[OFF_SQRT : OFF_SQRT + 16], "little"),
    }


def mint_risky(mint):
    v = rpc("getAccountInfo", [mint, {"encoding": "jsonParsed"}])["value"]
    if v is None:
        return None
    i = v["data"]["parsed"]["info"]
    return i.get("mintAuthority") is not None or i.get("freezeAuthority") is not None


def holders(mint, excludes):
    supply = int(rpc("getTokenSupply", [mint])["value"]["amount"])
    if supply == 0:
        return None
    largest = rpc("getTokenLargestAccounts", [mint])["value"]
    ex = set(excludes)
    amts = sorted((int(a["amount"]) for a in largest if a["address"] not in ex), reverse=True)
    if not amts:
        return {"supply": supply, "n": 0, "top": 0.0, "top20": 0.0}
    return {
        "supply": supply,
        "n": len(amts),
        "top": amts[0] / supply * 100.0,
        "top20": sum(amts[:20]) / supply * 100.0,
    }


# Holder kapısından geçen tokenler
TOKENS = [
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"),
    ("PYTH", "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3"),
    ("JTO", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"),
    ("WIF", "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm"),
    ("DRIFT", "DriFtupJYLTosbwoN8koMbEYSx54aFAVLddWsbksjwg7"),
]

print("== Raydium API: bu tokenlerin CLMM havuzları aranıyor ==")
found = []
for name, mint in TOKENS:
    for page in range(1, 4):
        api = (
            "https://api-v3.raydium.io/pools/info/mint"
            f"?mint1={mint}&mint2={WSOL}&poolType=concentrated"
            f"&poolSortField=volume24h&sortType=desc&pageSize=50&page={page}"
        )
        try:
            req = urllib.request.Request(api, headers={"User-Agent": "curl/8"})
            body = json.loads(urllib.request.urlopen(req, timeout=45).read())
        except Exception as e:  # noqa: BLE001
            print(f"  [warn] {name} page {page}: {e}")
            break
        data = body.get("data", {}).get("data", [])
        if not data:
            break
        for p in data:
            if p.get("programId") == CLMM:
                found.append((name, p["id"], p))
        time.sleep(0.3)

print(f"Bulunan CLMM havuzu: {len(found)}")
for name, pid, p in found:
    print(f"  {name:6} {pid}  tvl={p.get('tvl')}  vol24h={p.get('day',{}).get('volume')}")

print()
print("== Tüm kapılar test ediliyor ==")
results = []
for name, pid, meta in found:
    st = pool_state(pid)
    if not st:
        continue
    if st["mint0"] == WSOL:
        in_mint, out_mint, in_vault = st["mint0"], st["mint1"], st["vault0"]
    elif st["mint1"] == WSOL:
        in_mint, out_mint, in_vault = st["mint1"], st["mint0"], st["vault1"]
    else:
        continue

    rec = {"name": name, "pool_id": pid, "input_mint": in_mint, "output_mint": out_mint,
           "input_vault": in_vault, "tvl_usd": meta.get("tvl")}

    r_in = mint_risky(in_mint)
    r_out = mint_risky(out_mint)
    if r_in or r_out:
        rec["verdict"] = "REJECT authority"
        results.append(rec)
        continue

    liq = int(rpc("getTokenAccountBalance", [in_vault])["value"]["amount"])
    rec["liq_sol"] = round(liq / 1e9, 3)
    if liq < MIN_LIQ:
        rec["verdict"] = f"REJECT liq {liq/1e9:.1f} SOL"
        results.append(rec)
        continue

    hs = holders(in_mint, [st["vault0"], st["vault1"]])
    rec["holders"] = hs
    if not hs or hs["n"] == 0:
        rec["verdict"] = "REJECT no holders"
        results.append(rec)
        continue
    if hs["top"] > MAX_SINGLE:
        rec["verdict"] = f"REJECT top1 %{hs['top']:.1f}"
        results.append(rec)
        continue
    if hs["top20"] > MAX_TOP20:
        rec["verdict"] = f"REJECT top20 %{hs['top20']:.1f}"
        results.append(rec)
        continue

    # Fiyat hareketi
    s0 = pool_state(pid)["sqrt"]
    time.sleep(15)
    s1 = pool_state(pid)["sqrt"]
    rec["sqrt_t0"], rec["sqrt_t1"] = str(s0), str(s1)
    rec["moved"] = s0 != s1
    rec["delta_bps"] = round(abs(s1 - s0) / s0 * 10000, 3) if s0 else 0
    rec["verdict"] = "PASS" if s0 != s1 else "REJECT no price movement"
    results.append(rec)

print()
for r in results:
    hs = r.get("holders")
    hs_s = f"top1=%{hs['top']:.1f} top20=%{hs['top20']:.1f}" if hs else "n/a"
    print(f"{r['name']:6} {r['pool_id']}")
    print(f"       liq={r.get('liq_sol')} SOL  {hs_s}  moved={r.get('moved')} ({r.get('delta_bps')}bps)")
    print(f"       >>> {r['verdict']}")

with open("final_pool_results.json", "w", encoding="utf-8") as f:
    json.dump(results, f, indent=2, ensure_ascii=False)
print("\nYazıldı: final_pool_results.json")
