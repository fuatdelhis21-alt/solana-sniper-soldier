"""Aday havuzları derinlemesine kontrol eder: holder kapısı + uzun fiyat hareketi."""
import json
import sys
import time
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"
WSOL = "So11111111111111111111111111111111111111112"

OFF_MINT0, OFF_MINT1 = 73, 105
OFF_VAULT0, OFF_VAULT1 = 137, 169
OFF_SQRT = 253

MIN_LIQ = 1_000_000_000_000
MAX_SINGLE, MAX_TOP20 = 30.0, 70.0


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
    import base64

    d = base64.b64decode(v["data"][0])
    return {
        "mint0": b58e(d[OFF_MINT0 : OFF_MINT0 + 32]),
        "mint1": b58e(d[OFF_MINT1 : OFF_MINT1 + 32]),
        "vault0": b58e(d[OFF_VAULT0 : OFF_VAULT0 + 32]),
        "vault1": b58e(d[OFF_VAULT1 : OFF_VAULT1 + 32]),
        "sqrt": int.from_bytes(d[OFF_SQRT : OFF_SQRT + 16], "little"),
    }


def holder_stats(mint, excludes):
    supply = int(rpc("getTokenSupply", [mint])["value"]["amount"])
    if supply == 0:
        return None
    largest = rpc("getTokenLargestAccounts", [mint])["value"]
    ex = set(excludes)
    amts = sorted(
        (int(a["amount"]) for a in largest if a["address"] not in ex), reverse=True
    )
    if not amts:
        return {"supply": supply, "sampled": 0, "top_pct": 0.0, "top20_pct": 0.0}
    return {
        "supply": supply,
        "sampled": len(amts),
        "top_pct": amts[0] / supply * 100.0,
        "top20_pct": sum(amts[:20]) / supply * 100.0,
    }


def main():
    pools = sys.argv[1:]
    for pid in pools:
        print("=" * 78)
        print(f"HAVUZ {pid}")
        st = pool_state(pid)
        if not st:
            print("  okunamadı")
            continue
        if st["mint0"] == WSOL:
            in_mint, out_mint, in_vault = st["mint0"], st["mint1"], st["vault0"]
        else:
            in_mint, out_mint, in_vault = st["mint1"], st["mint0"], st["vault1"]
        print(f"  input_mint  = {in_mint}")
        print(f"  output_mint = {out_mint}")
        print(f"  input_vault = {in_vault}")

        liq = int(rpc("getTokenAccountBalance", [in_vault])["value"]["amount"])
        print(f"  likidite    = {liq} lamports = {liq/1e9:.3f} SOL  (>=1000 SOL: {liq >= MIN_LIQ})")

        hs = holder_stats(in_mint, [st["vault0"], st["vault1"]])
        print(f"  holder      = {hs}")
        if hs and hs["sampled"] > 0:
            print(f"    top1  %{hs['top_pct']:.2f} <= 30 : {hs['top_pct'] <= MAX_SINGLE}")
            print(f"    top20 %{hs['top20_pct']:.2f} <= 70 : {hs['top20_pct'] <= MAX_TOP20}")

        # Uzun fiyat hareketi ölçümü
        print("  --- 60 sn fiyat hareketi ölçümü (6 örnek, 10 sn arayla) ---")
        samples = []
        for i in range(6):
            s = pool_state(pid)
            samples.append(s["sqrt"])
            print(f"    t={i*10:3}s sqrt={s['sqrt']}")
            if i < 5:
                time.sleep(10)
        uniq = len(set(samples))
        lo, hi = min(samples), max(samples)
        rng_bps = (hi - lo) / lo * 10000 if lo else 0
        print(f"  BENZERSİZ sqrt_price: {uniq}/6   aralık={rng_bps:.2f} bps")
        print(f"  >>> FİYAT HAREKETİ VAR MI: {uniq > 1}")


if __name__ == "__main__":
    main()
