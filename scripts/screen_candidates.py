#!/usr/bin/env python3
"""Screen tokens for the sniper's on-chain risk gates (stdlib only).

For each mint: find the most liquid Raydium CLMM pool paired with wSOL,
then apply the same checks the bot applies before an entry:
  - classic SPL Token program (Token-2022 is not supported by the swap path)
  - mint authority and freeze authority both revoked
  - holder concentration of the top-20 accounts, excluding the pool's own
    vaults and program-controlled (PDA-owned) accounts:
    single holder <= 30 %, top-20 combined <= 70 %
  - SOL-side vault liquidity >= 1000 SOL (strategy minimum)

Usage (RPC is read from the bot's env file unless SNIPER_RPC is set):
  python3 scripts/screen_candidates.py [MINT ...]
Without arguments a default list of well-known tokens is screened.
"""
import base64
import json
import os
import sys
import time
import urllib.request

WSOL = "So11111111111111111111111111111111111111112"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
MAX_SINGLE, MAX_TOP20, MIN_LIQ_SOL = 30.0, 70.0, 1000.0

DEFAULT = {
    "BONK": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    "WIF": "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",
    "JUP": "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",
    "POPCAT": "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    "RAY": "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
    "JTO": "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL",
    "PYTH": "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3",
    "ORCA": "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE",
}

# ── base58 + ed25519 on-curve check (a PDA is off-curve) ──
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58decode(s):
    n = 0
    for c in s:
        n = n * 58 + B58.index(c)
    raw = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    pad = len(s) - len(s.lstrip("1"))
    return b"\0" * pad + raw


def b58encode(b):
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    pad = len(b) - len(b.lstrip(b"\0"))
    return "1" * pad + out


P = 2**255 - 19
D = (-121665 * pow(121666, P - 2, P)) % P


def on_curve(pk32):
    y = int.from_bytes(pk32, "little") & ((1 << 255) - 1)
    if y >= P:
        return False
    y2 = y * y % P
    x2 = (y2 - 1) * pow(D * y2 + 1, P - 2, P) % P
    if x2 == 0:
        return not (pk32[31] >> 7)
    return pow(x2, (P - 1) // 2, P) == 1


# ── RPC ──
def rpc_url():
    if os.environ.get("SNIPER_RPC"):
        return os.environ["SNIPER_RPC"]
    try:
        for line in open("/etc/solana-hft/solana-hft.env"):
            if line.startswith("SNIPER_RPC="):
                return line.split("=", 1)[1].strip()
    except OSError:
        pass
    return "https://api.mainnet-beta.solana.com"


RPC = rpc_url()
# Raydium's API (Cloudflare) answers 403 to the default "Python-urllib" agent.
HEADERS = {"content-type": "application/json", "user-agent": "Mozilla/5.0 (screen_candidates)"}


def rpc(method, params, retries=3):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for attempt in range(retries):
        req = urllib.request.Request(RPC, body, HEADERS)
        try:
            with urllib.request.urlopen(req, timeout=20) as r:
                d = json.load(r)
            if "result" in d:
                return d["result"]
            err = d.get("error")
        except Exception as e:  # noqa: BLE001
            err = "RPC " + str(e).split("api-key")[0]
        time.sleep(1.5 * (attempt + 1))
    raise RuntimeError(f"{method}: {err}")


def http_json(url):
    req = urllib.request.Request(url, headers=HEADERS)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.load(r)
    except Exception as e:  # noqa: BLE001
        raise RuntimeError(f"Raydium API: {e}") from None


def best_pool(mint):
    url = (
        "https://api-v3.raydium.io/pools/info/mint?mint1=" + WSOL + "&mint2=" + mint
        + "&poolType=concentrated&poolSortField=liquidity&sortType=desc&pageSize=1&page=1"
    )
    data = http_json(url).get("data", {}).get("data", [])
    return data[0] if data else None


def screen(sym, mint):
    pool = best_pool(mint)
    if not pool:
        return f"{sym:7} no Raydium CLMM/wSOL pool"
    pool_id = pool["id"]
    info = rpc("getAccountInfo", [mint, {"encoding": "jsonParsed"}])["value"]
    mi = info["data"]["parsed"]["info"]
    reasons = []
    if info["owner"] != TOKEN_PROGRAM:
        reasons.append("Token-2022")
    if mi["mintAuthority"] is not None:
        reasons.append("mint-authority")
    if mi["freezeAuthority"] is not None:
        reasons.append("freeze-authority")

    pstate = rpc("getAccountInfo", [pool_id, {"encoding": "base64"}])["value"]
    raw = base64.b64decode(pstate["data"][0])
    # CLMM PoolState: 8 disc + bump(1) + amm_config(32) + owner(32) +
    # mint0(32) + mint1(32) + vault0(32) + vault1(32)
    off = 8 + 1 + 32 + 32
    mint0 = b58encode(raw[off:off + 32])
    v0 = b58encode(raw[off + 64:off + 96])
    v1 = b58encode(raw[off + 96:off + 128])
    sol_vault = v0 if mint0 == WSOL else v1
    liq = int(rpc("getTokenAccountBalance", [sol_vault])["value"]["amount"]) / 1e9
    if liq < MIN_LIQ_SOL:
        reasons.append(f"liq {liq:.0f} SOL < {MIN_LIQ_SOL:.0f}")

    supply = int(rpc("getTokenSupply", [mint])["value"]["amount"])
    largest = rpc("getTokenLargestAccounts", [mint])["value"]
    addrs = [a["address"] for a in largest]
    accs = rpc("getMultipleAccounts", [addrs, {"encoding": "base64"}])["value"]
    top = comb = prog = 0
    for a, acc in zip(largest, accs):
        amt = int(a["amount"])
        if a["address"] in (v0, v1):
            continue
        if acc:
            data = base64.b64decode(acc["data"][0])
            if len(data) >= 64 and not on_curve(data[32:64]):
                prog += amt
                continue
        comb += amt
        top = max(top, amt)
    t1, t20, pp = 100 * top / supply, 100 * comb / supply, 100 * prog / supply
    if t1 > MAX_SINGLE:
        reasons.append(f"top1 {t1:.1f}%")
    if t20 > MAX_TOP20:
        reasons.append(f"top20 {t20:.1f}%")
    verdict = "PASS" if not reasons else "FAIL: " + ", ".join(reasons)
    return (
        f"{sym:7} pool={pool_id} liq={liq:,.0f} SOL top1={t1:.1f}% top20={t20:.1f}% "
        f"pda={pp:.1f}%  -> {verdict}"
    )


def main():
    targets = {m[:6]: m for m in sys.argv[1:]} or DEFAULT
    print(f"RPC: {RPC.split('?')[0]}  (thresholds: top1<={MAX_SINGLE}% top20<={MAX_TOP20}% liq>={MIN_LIQ_SOL} SOL)")
    for sym, mint in targets.items():
        try:
            print(screen(sym, mint), flush=True)
        except Exception as e:  # noqa: BLE001
            print(f"{sym:7} error: {str(e)[:160]}", flush=True)
        time.sleep(1)


if __name__ == "__main__":
    main()
