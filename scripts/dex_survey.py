#!/usr/bin/env python3
"""Read-only survey: which Solana DEX lists new SOL pools that are liquid and
pass the bot's mint checks? Input for choosing the next DEX to support.

Nothing is signed or sent. Uses GeckoTerminal's public new-pools listing and,
optionally, one RPC call (getAccountInfo of the token mint) per pool that
reached the liquidity threshold. The RPC URL is read from the env file and is
never printed or written.

Run (24 h, in the background, survives logout):
  systemd-run --unit=dex-survey /usr/bin/python3 -I \
      /root/solana-sniper-soldier/scripts/dex_survey.py --hours 24
Summary (any time, also while it runs):
  python3 -I scripts/dex_survey.py --summary

Stdlib only.
"""
import argparse
import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime

WSOL = "So11111111111111111111111111111111111111112"
SPL_TOKEN = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
TOKEN_2022 = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
# Same allow-list as the bot (onchain_risk.rs): metadata pointer + metadata.
T22_ALLOWED = {18, 19}
T22_NAMES = {
    1: "TransferFeeConfig", 3: "MintCloseAuthority", 4: "ConfidentialTransferMint",
    6: "DefaultAccountState", 9: "NonTransferable", 10: "InterestBearingConfig",
    12: "PermanentDelegate", 14: "TransferHook", 16: "ConfidentialTransferFeeConfig",
    18: "MetadataPointer", 19: "TokenMetadata", 20: "GroupPointer", 21: "TokenGroup",
    22: "GroupMemberPointer", 23: "TokenGroupMember", 24: "ConfidentialMintBurn",
    25: "ScaledUiAmount", 26: "Pausable",
}
GECKO_NEW = "https://api.geckoterminal.com/api/v2/networks/solana/new_pools?page={page}"
GECKO_MULTI = "https://api.geckoterminal.com/api/v2/networks/solana/pools/multi/{addrs}"
MULTI_MAX = 30


def parse_time_ms(s):
    try:
        return int(datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() * 1000)
    except (AttributeError, ValueError):
        return None


def rel_id(item, name):
    """Address from a relationship id such as "solana_<address>"."""
    try:
        v = item["relationships"][name]["data"]["id"]
    except (KeyError, TypeError):
        return None
    if not isinstance(v, str):
        return None
    return v.split("_", 1)[1] if v.startswith("solana_") else v


def to_float(v):
    try:
        f = float(v)
    except (TypeError, ValueError):
        return None
    return f if f == f and f not in (float("inf"), float("-inf")) else None


def parse_pool(item):
    """One listing entry -> dict, or None when malformed."""
    a = item.get("attributes") if isinstance(item, dict) else None
    if not isinstance(a, dict):
        return None
    address = a.get("address")
    created = parse_time_ms(a.get("pool_created_at"))
    dex = rel_id(item, "dex")
    base, quote = rel_id(item, "base_token"), rel_id(item, "quote_token")
    if not (isinstance(address, str) and created and dex and base and quote):
        return None
    sol_price = None
    if quote == WSOL:
        sol_price = to_float(a.get("quote_token_price_usd"))
    elif base == WSOL:
        sol_price = to_float(a.get("base_token_price_usd"))
    return {
        "pool": address, "dex": dex, "created_ms": created,
        "mint": base if quote == WSOL else quote if base == WSOL else None,
        "reserve_usd": to_float(a.get("reserve_in_usd")),
        "sol_price": sol_price,
    }


def parse_listing(body):
    """(pools, malformed count). Raises ValueError for a non-listing body."""
    v = json.loads(body)
    data = v.get("data") if isinstance(v, dict) else None
    if not isinstance(data, list):
        raise ValueError("response has no data array")
    pools, bad = [], 0
    for item in data:
        p = parse_pool(item)
        if p is None:
            bad += 1
        else:
            pools.append(p)
    return pools, bad


def sol_side_estimate(reserve_usd, sol_price):
    """SOL-side liquidity estimate: half the pool's USD reserve in SOL. An
    approximation (concentrated / one-sided pools are not 50/50)."""
    if reserve_usd is None or not sol_price or sol_price <= 0:
        return None
    return reserve_usd / 2 / sol_price


def classify_mint(owner, data):
    """Mirror of the bot's mint checks (paper policy with Token-2022 on).
    Returns (verdict, detail)."""
    if owner == SPL_TOKEN:
        if len(data) != 82:
            return "malformed", "classic mint size"
        program = "classic"
    elif owner == TOKEN_2022:
        program = "token2022"
        if len(data) != 82:
            if len(data) <= 165 or any(data[82:165]) or data[165] != 1:
                return "malformed", "token-2022 layout"
            i = 166
            while i < len(data):
                if len(data) - i < 4:
                    return "malformed", "token-2022 tlv"
                ty = int.from_bytes(data[i:i + 2], "little")
                ln = int.from_bytes(data[i + 2:i + 4], "little")
                if ty == 0:
                    if any(data[i:]):
                        return "malformed", "token-2022 tlv"
                    break
                if ty not in T22_ALLOWED:
                    return "bad_extension", T22_NAMES.get(ty, f"unknown({ty})")
                i += 4 + ln
                if i > len(data):
                    return "malformed", "token-2022 tlv"
    else:
        return "other_program", owner
    if data[45] != 1:
        return "malformed", "uninitialized"
    mint_tag = int.from_bytes(data[0:4], "little")
    freeze_tag = int.from_bytes(data[46:50], "little")
    if mint_tag > 1 or freeze_tag > 1:
        return "malformed", "authority option"
    if mint_tag or freeze_tag:
        return "authority", program
    return "ok_" + program, ""


def read_rpc_url(env_file):
    try:
        with open(env_file) as f:
            for line in f:
                line = line.strip()
                if line.startswith("SNIPER_RPC="):
                    return line.split("=", 1)[1].strip().strip('"').strip("'") or None
    except OSError:
        return None
    return None


def redact(msg, secret):
    msg = str(msg)
    if secret:
        msg = msg.replace(secret, "<rpc>")
        host = secret.split("://", 1)[-1].split("/", 1)[0]
        if host:
            msg = msg.replace(host, "<rpc-host>")
    return msg[:200]


def http_get(url, timeout=20):
    req = urllib.request.Request(url, headers={"Accept": "application/json",
                                               "User-Agent": "dex-survey/1"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode()


def rpc_account(rpc, mint):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
                       "params": [mint, {"encoding": "base64"}]}).encode()
    req = urllib.request.Request(rpc, data=body,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=20) as r:
        v = json.loads(r.read().decode())
    if "error" in v:
        raise RuntimeError(f"rpc error {v['error'].get('code')}")
    val = (v.get("result") or {}).get("value")
    if not val:
        raise RuntimeError("mint account not found")
    return val["owner"], base64.b64decode(val["data"][0])


def append(out, rec):
    with open(out, "a") as f:
        f.write(json.dumps(rec, sort_keys=True) + "\n")


def run(args):
    rpc = None if args.no_rpc else read_rpc_url(args.env_file)
    tracked, seen = {}, set()
    sol_price, last_poll_ms = None, None
    end = time.time() + args.hours * 3600
    append(args.out, {"kind": "start", "t": int(time.time() * 1000),
                      "min_sol": args.min_sol, "check_after_min": args.check_after_min,
                      "rpc": bool(rpc)})
    while time.time() < end:
        now = int(time.time() * 1000)
        oldest, ok_pages = None, 0
        for page in range(1, args.pages + 1):
            try:
                pools, bad = parse_listing(http_get(GECKO_NEW.format(page=page)))
            except (urllib.error.URLError, OSError, ValueError) as e:
                append(args.out, {"kind": "gecko_error", "t": now, "err": str(e)[:120]})
                time.sleep(5)
                continue
            ok_pages += 1
            for p in pools:
                oldest = p["created_ms"] if oldest is None else min(oldest, p["created_ms"])
                if p["sol_price"]:
                    sol_price = p["sol_price"]
                if p["pool"] in seen:
                    continue
                seen.add(p["pool"])
                rec = {"kind": "new", "t": now, "pool": p["pool"], "dex": p["dex"],
                       "created_ms": p["created_ms"], "sol_pair": p["mint"] is not None,
                       "reserve_usd": p["reserve_usd"]}
                append(args.out, rec)
                if p["mint"] and now - p["created_ms"] <= args.check_after_min * 60_000:
                    tracked[p["pool"]] = dict(p, first_seen=now)
            time.sleep(2)  # stay well under the public API rate limit
        if ok_pages == args.pages and last_poll_ms and oldest and oldest > last_poll_ms:
            append(args.out, {"kind": "gap", "t": now})
        if ok_pages:
            last_poll_ms = now
        due = [p for p in tracked.values()
               if now - p["first_seen"] >= args.check_after_min * 60_000]
        for i in range(0, len(due), MULTI_MAX):
            chunk = due[i:i + MULTI_MAX]
            try:
                fresh, _ = parse_listing(http_get(GECKO_MULTI.format(
                    addrs=",".join(p["pool"] for p in chunk))))
            except (urllib.error.URLError, OSError, ValueError) as e:
                append(args.out, {"kind": "gecko_error", "t": now, "err": str(e)[:120]})
                continue
            by = {p["pool"]: p for p in fresh}
            for p in chunk:
                tracked.pop(p["pool"], None)
                f = by.get(p["pool"])
                est = sol_side_estimate(f["reserve_usd"] if f else None, sol_price)
                rec = {"kind": "checked", "t": now, "pool": p["pool"], "dex": p["dex"],
                       "mint": p["mint"], "sol_est": est,
                       "liquid": est is not None and est >= args.min_sol}
                if rec["liquid"] and rpc:
                    try:
                        owner, data = rpc_account(rpc, p["mint"])
                        rec["verdict"], rec["detail"] = classify_mint(owner, data)
                    except Exception as e:  # noqa: BLE001 — recorded, never fatal
                        rec["verdict"], rec["detail"] = "rpc_error", redact(e, rpc)
                    time.sleep(0.5)
                append(args.out, rec)
            time.sleep(2)
        time.sleep(max(0, args.poll_secs - args.pages * 2))
    append(args.out, {"kind": "end", "t": int(time.time() * 1000)})


def summarize(lines):
    """Per-DEX table from the survey records (list of dicts)."""
    stats, gaps, errors, start = {}, 0, 0, None
    for r in lines:
        k = r.get("kind")
        if k == "start":
            start = start or r
        elif k == "gap":
            gaps += 1
        elif k == "gecko_error":
            errors += 1
        elif k in ("new", "checked"):
            s = stats.setdefault(r.get("dex", "?"), {
                "new": 0, "sol": 0, "checked": 0, "liquid": 0, "ok_classic": 0,
                "ok_token2022": 0, "authority": 0, "bad_extension": 0, "other": 0})
            if k == "new":
                s["new"] += 1
                s["sol"] += bool(r.get("sol_pair"))
            else:
                s["checked"] += 1
                if r.get("liquid"):
                    s["liquid"] += 1
                    v = r.get("verdict")
                    if v in ("ok_classic", "ok_token2022", "authority", "bad_extension"):
                        s[v] += 1
                    elif v is not None:
                        s["other"] += 1
    out = []
    if start:
        out.append(f"threshold: SOL side >= {start['min_sol']} SOL (estimate: reserve/2), "
                   f"checked {start['check_after_min']} min after listing; "
                   f"mint check: {'on' if start['rpc'] else 'OFF (no RPC)'}")
    out.append(f"{'dex':<22}{'new':>6}{'SOL':>6}{'chk':>6}{'liquid':>8}"
               f"{'ok':>6}{'ok22':>6}{'auth':>6}{'ext':>6}{'oth':>6}")
    for dex, s in sorted(stats.items(), key=lambda kv: -kv[1]["liquid"]):
        out.append(f"{dex:<22}{s['new']:>6}{s['sol']:>6}{s['checked']:>6}{s['liquid']:>8}"
                   f"{s['ok_classic']:>6}{s['ok_token2022']:>6}{s['authority']:>6}"
                   f"{s['bad_extension']:>6}{s['other']:>6}")
    out.append(f"listing gaps (pools may have been missed): {gaps}; GeckoTerminal errors: {errors}")
    out.append("ok = classic SPL, ok22 = Token-2022 metadata-only, both with revoked "
               "authorities. Holder concentration is NOT measured here; the bot's "
               "top-20 > 70 % gate rejects more.")
    return "\n".join(out)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--out", default="/var/lib/solana-hft/dex_survey.jsonl")
    ap.add_argument("--summary", action="store_true", help="print the summary and exit")
    ap.add_argument("--hours", type=float, default=24)
    ap.add_argument("--poll-secs", type=int, default=60)
    ap.add_argument("--pages", type=int, default=5)
    ap.add_argument("--min-sol", type=float, default=50)
    ap.add_argument("--check-after-min", type=int, default=30)
    ap.add_argument("--env-file", default="/etc/solana-hft/solana-hft.env")
    ap.add_argument("--no-rpc", action="store_true", help="skip the mint check")
    args = ap.parse_args(argv)
    if args.summary:
        try:
            with open(args.out) as f:
                lines = [json.loads(l) for l in f if l.strip()]
        except (OSError, ValueError) as e:
            print(f"cannot read {args.out}: {e}", file=sys.stderr)
            return 1
        print(summarize(lines))
        return 0
    if args.poll_secs < 30 or not 1 <= args.pages <= 10 or args.hours <= 0:
        print("--poll-secs >= 30, --pages 1..10, --hours > 0", file=sys.stderr)
        return 2
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    run(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
