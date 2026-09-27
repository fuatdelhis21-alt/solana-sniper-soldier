#!/usr/bin/env python3
"""
AŞAMA 2 havuz seçici — gerçek fiyat hareketi olan, rug-check'ten geçen
Raydium CLMM mainnet havuzu bulur.

Kriterler (kodun gerçek kapılarıyla birebir):
  1. Fiyat hareketi: iki örnekleme arasında sqrt_price_x64 DEĞİŞMELİ
  2. Mint/freeze authority YOK (fetch_mint_authority_risk -> is_risky)
  3. Likidite >= 1000 SOL (min_liquidity_lamports = 1e12)
     -> input vault bakiyesi (fetch_vault_liquidity)
  4. Holder yoğunlaşması: tek holder <= %30, top-20 <= %70
  5. wSOL bir tarafta olması tercih edilir

Kullanım:
  python scripts/find_paper_pool.py --rpc "<HELIUS_URL>" [--top 40]
"""
import argparse
import json
import sys
import time
import urllib.request

CLMM_PROGRAM = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"
WSOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
USDT = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"

# parse_pool_state offsets (account-relative, 8-byte anchor discriminator)
OFF_MINT0 = 73
OFF_MINT1 = 105
OFF_VAULT0 = 137
OFF_VAULT1 = 169
OFF_LIQUIDITY = 237
OFF_SQRT_PRICE = 253

MIN_LIQ_LAMPORTS = 1_000_000_000_000  # 1000 SOL
MAX_SINGLE_HOLDER_PCT = 30.0
MAX_TOP20_HOLDER_PCT = 70.0


def rpc(url, method, params):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    ).encode()
    req = urllib.request.Request(
        url, data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=45) as resp:
        out = json.loads(resp.read())
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error']}")
    return out["result"]


def b58_decode(s):
    alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    n = 0
    for ch in s:
        n = n * 58 + alphabet.index(ch)
    raw = n.to_bytes((n.bit_length() + 7) // 8, "big")
    pad = len(s) - len(s.lstrip("1"))
    return b"\x00" * pad + raw


def b58_encode(b):
    alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = alphabet[r] + out
    pad = len(b) - len(b.lstrip(b"\x00"))
    return "1" * pad + (out or "")


def read_pubkey(data, off):
    return b58_encode(data[off : off + 32])


def read_u128(data, off):
    return int.from_bytes(data[off : off + 16], "little")


def get_accounts(url, keys):
    """getMultipleAccounts — base64."""
    res = rpc(url, "getMultipleAccounts", [keys, {"encoding": "base64"}])
    return res["value"]


def parse_pool(data):
    return {
        "mint0": read_pubkey(data, OFF_MINT0),
        "mint1": read_pubkey(data, OFF_MINT1),
        "vault0": read_pubkey(data, OFF_VAULT0),
        "vault1": read_pubkey(data, OFF_VAULT1),
        "liquidity": read_u128(data, OFF_LIQUIDITY),
        "sqrt_price": read_u128(data, OFF_SQRT_PRICE),
    }


def mint_authority_risky(url, mint):
    """fetch_mint_authority_risk eşdeğeri: mint veya freeze authority varsa riskli."""
    res = rpc(url, "getAccountInfo", [mint, {"encoding": "jsonParsed"}])
    val = res["value"]
    if val is None:
        return None, "mint account not found"
    info = val["data"]["parsed"]["info"]
    mint_auth = info.get("mintAuthority")
    freeze_auth = info.get("freezeAuthority")
    return (mint_auth is not None or freeze_auth is not None), {
        "mint_authority": mint_auth,
        "freeze_authority": freeze_auth,
    }


def vault_balance(url, vault):
    res = rpc(url, "getTokenAccountBalance", [vault])
    return int(res["value"]["amount"])


def holder_stats(url, mint, excludes):
    """fetch_holder_stats eşdeğeri: supply + largest accounts, vault'lar hariç."""
    supply = int(rpc(url, "getTokenSupply", [mint])["value"]["amount"])
    if supply == 0:
        return None
    largest = rpc(url, "getTokenLargestAccounts", [mint])["value"]
    ex = set(excludes)
    amounts = []
    for acc in largest:
        if acc["address"] in ex:
            continue
        amounts.append(int(acc["amount"]))
    if not amounts:
        return {"supply": supply, "sampled": 0, "top_pct": 0.0, "top20_pct": 0.0}
    amounts.sort(reverse=True)
    top_pct = amounts[0] / supply * 100.0
    top20_pct = sum(amounts[:20]) / supply * 100.0
    return {
        "supply": supply,
        "sampled": len(amounts),
        "top_pct": top_pct,
        "top20_pct": top20_pct,
    }


def fetch_candidates(url, pages=3):
    """Raydium API'den mainnet CLMM havuzlarını çek."""
    pools = []
    for page in range(1, pages + 1):
        api = (
            "https://api-v3.raydium.io/pools/info/list"
            f"?poolType=concentrated&poolSortField=volume24h&sortType=desc"
            f"&pageSize=100&page={page}"
        )
        try:
            req = urllib.request.Request(api, headers={"User-Agent": "curl/8"})
            with urllib.request.urlopen(req, timeout=45) as resp:
                body = json.loads(resp.read())
        except Exception as e:  # noqa: BLE001
            print(f"[warn] Raydium API page {page} failed: {e}", file=sys.stderr)
            break
        data = body.get("data", {}).get("data", [])
        if not data:
            break
        pools.extend(data)
        time.sleep(0.4)
    return pools


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rpc", required=True)
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--pages", type=int, default=3)
    ap.add_argument("--out", default="pool_candidates.json")
    args = ap.parse_args()

    print("== Raydium CLMM mainnet havuz listesi çekiliyor ==")
    raw = fetch_candidates(args.rpc, args.pages)
    print(f"API'den {len(raw)} havuz geldi")

    # Sadece mainnet CLMM + wSOL/USDC/USDT eşleşmeleri
    cands = []
    for p in raw:
        if p.get("programId") != CLMM_PROGRAM:
            continue
        mints = {p.get("mintA", {}).get("address"), p.get("mintB", {}).get("address")}
        if WSOL not in mints:
            continue
        cands.append(p)
    print(f"wSOL içeren mainnet CLMM havuzu: {len(cands)}")

    cands = cands[: args.top]
    ids = [p["id"] for p in cands]

    print(f"== {len(ids)} havuz için on-chain durum okunuyor (t=0) ==")
    t0 = {}
    for i in range(0, len(ids), 100):
        chunk = ids[i : i + 100]
        vals = get_accounts(args.rpc, chunk)
        for pid, v in zip(chunk, vals):
            if v is None:
                continue
            data = __import__("base64").b64decode(v["data"][0])
            if len(data) < 269:
                continue
            t0[pid] = parse_pool(data)

    print(f"t=0 okundu: {len(t0)} havuz. 20 sn bekleniyor (fiyat hareketi ölçümü)...")
    time.sleep(20)

    print("== t=1 okunuyor ==")
    ids2 = list(t0.keys())
    t1 = {}
    for i in range(0, len(ids2), 100):
        chunk = ids2[i : i + 100]
        vals = get_accounts(args.rpc, chunk)
        for pid, v in zip(chunk, vals):
            if v is None:
                continue
            data = __import__("base64").b64decode(v["data"][0])
            if len(data) < 269:
                continue
            t1[pid] = parse_pool(data)

    moved = []
    for pid, a in t0.items():
        b = t1.get(pid)
        if not b:
            continue
        if a["sqrt_price"] != b["sqrt_price"]:
            delta_bps = abs(b["sqrt_price"] - a["sqrt_price"]) / a["sqrt_price"] * 10000
            moved.append((pid, a, b, delta_bps))

    moved.sort(key=lambda x: -x[3])
    print(f"\n== 20 sn içinde fiyatı HAREKET EDEN havuz: {len(moved)} ==")

    results = []
    for pid, a, b, delta_bps in moved:
        meta = next((p for p in cands if p["id"] == pid), {})
        rec = {
            "pool_id": pid,
            "mint0": a["mint0"],
            "mint1": a["mint1"],
            "vault0": a["vault0"],
            "vault1": a["vault1"],
            "sqrt_t0": str(a["sqrt_price"]),
            "sqrt_t1": str(b["sqrt_price"]),
            "delta_bps_20s": round(delta_bps, 2),
            "api_volume24h_usd": meta.get("day", {}).get("volume"),
            "api_tvl_usd": meta.get("tvl"),
            "api_name": f"{meta.get('mintA', {}).get('symbol')}/{meta.get('mintB', {}).get('symbol')}",
        }
        results.append(rec)

    # On-chain kapı kontrolleri
    print("\n== On-chain kapı kontrolleri (authority / likidite / holder) ==")
    passed = []
    for rec in results:
        pid = rec["pool_id"]
        try:
            # wSOL tarafını input kabul et
            if rec["mint0"] == WSOL:
                input_mint, output_mint = rec["mint0"], rec["mint1"]
                input_vault = rec["vault0"]
            elif rec["mint1"] == WSOL:
                input_mint, output_mint = rec["mint1"], rec["mint0"]
                input_vault = rec["vault1"]
            else:
                continue
            rec["input_mint"] = input_mint
            rec["output_mint"] = output_mint
            rec["input_vault"] = input_vault

            risky_in, info_in = mint_authority_risky(args.rpc, input_mint)
            risky_out, info_out = mint_authority_risky(args.rpc, output_mint)
            rec["authority_in"] = info_in
            rec["authority_out"] = info_out
            if risky_in is None or risky_out is None:
                rec["verdict"] = "SKIP (mint okunamadı)"
                continue
            if risky_in or risky_out:
                rec["verdict"] = "REJECT (mint/freeze authority var)"
                continue

            liq = vault_balance(args.rpc, input_vault)
            rec["input_vault_lamports"] = liq
            rec["input_vault_sol"] = round(liq / 1e9, 3)
            if liq < MIN_LIQ_LAMPORTS:
                rec["verdict"] = f"REJECT (likidite {liq/1e9:.1f} SOL < 1000 SOL)"
                continue

            hs = holder_stats(args.rpc, input_mint, [rec["vault0"], rec["vault1"]])
            rec["holder_stats"] = hs
            if hs is None:
                rec["verdict"] = "REJECT (supply=0)"
                continue
            if hs["sampled"] == 0:
                rec["verdict"] = "REJECT (ölçülebilir holder yok — fail-closed)"
                continue
            if hs["top_pct"] > MAX_SINGLE_HOLDER_PCT:
                rec["verdict"] = f"REJECT (tek holder %{hs['top_pct']:.1f} > 30)"
                continue
            if hs["top20_pct"] > MAX_TOP20_HOLDER_PCT:
                rec["verdict"] = f"REJECT (top20 %{hs['top20_pct']:.1f} > 70)"
                continue

            rec["verdict"] = "PASS"
            passed.append(rec)
        except Exception as e:  # noqa: BLE001
            rec["verdict"] = f"ERROR: {e}"
        time.sleep(0.15)

    print(f"\n== TÜM KAPILARDAN GEÇEN HAVUZ: {len(passed)} ==")
    for r in passed:
        print(
            f"  {r['pool_id']}  {r['api_name']}  "
            f"hareket={r['delta_bps_20s']}bps/20s  "
            f"likidite={r['input_vault_sol']} SOL  "
            f"top1=%{r['holder_stats']['top_pct']:.1f}  "
            f"top20=%{r['holder_stats']['top20_pct']:.1f}"
        )

    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(
            {"passed": passed, "all_moved": results}, f, indent=2, ensure_ascii=False
        )
    print(f"\nSonuçlar yazıldı: {args.out}")


if __name__ == "__main__":
    main()
