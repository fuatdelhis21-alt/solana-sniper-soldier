#!/usr/bin/env python3
"""Summarize the bot's decision log across all pools (stdlib only).

pool-sniper runs one solana-sniper process per pool, so every process only
prints its own summary. All of them append to the same decision log
(<data_dir>/decisions/decisions.jsonl); this script aggregates it.

Usage:
  python3 scripts/sniper_report.py [DATA_DIR] [--hours N]
DATA_DIR defaults to /var/lib/solana-hft; --hours limits to the last N hours.
"""
import collections
import json
import re
import sys
import time

args = [a for a in sys.argv[1:]]
hours = None
if "--hours" in args:
    i = args.index("--hours")
    hours = float(args[i + 1])
    del args[i:i + 2]
data_dir = args[0] if args else "/var/lib/solana-hft"
path = f"{data_dir}/decisions/decisions.jsonl"
since_ms = (time.time() - hours * 3600) * 1000 if hours else 0

actions = collections.Counter()
reasons = collections.Counter()
pools = collections.defaultdict(lambda: {"mode": "", "enter": 0, "exit": 0, "pnl": 0, "last": ""})
pnl_by_mode = collections.Counter()
lines = bad = 0

try:
    f = open(path)
except OSError as e:
    sys.exit(f"cannot open {path}: {e}")
with f:
    for line in f:
        try:
            r = json.loads(line)
        except ValueError:
            bad += 1
            continue
        if r.get("decision_time_ms", 0) < since_ms:
            continue
        lines += 1
        mode = r.get("mode", "?")
        ctx = r.get("context") or {}
        action = ctx.get("action") if isinstance(ctx, dict) else None
        if action is None:
            action = "evaluated" if mode in ("live", "dry_run") else "other"
        actions[(mode, action)] += 1
        p = pools[r.get("pool_id", "")]
        p["mode"] = mode
        p["last"] = action
        if action in ("rejected", "data_error"):
            reason = ctx.get("reject_reason") or ctx.get("error") or "?"
            reasons[(mode, str(reason)[:90])] += 1
        if action == "enter":
            p["enter"] += 1
        pnl = None
        if action.startswith("exit_"):
            pnl = ctx.get("realized_simulated_pnl_lamports")
        elif action == "exit":
            m = re.search(r"pnl: (-?\d+)", str(ctx.get("outcome", "")))
            pnl = int(m.group(1)) if m else None
        if pnl is not None:
            p["exit"] += 1
            p["pnl"] += pnl
            pnl_by_mode[mode] += pnl

span = f"last {hours:g} h" if hours else "all time"
print(f"{path} — {lines} records ({span}){f', {bad} unreadable' if bad else ''}")
print("\nActions:")
for (mode, action), n in sorted(actions.items()):
    print(f"  {mode:8} {action:22} {n}")
if reasons:
    print("\nTop rejection / data-error reasons:")
    for (mode, reason), n in reasons.most_common(10):
        print(f"  {n:5}  {mode:8} {reason}")
traded = {k: v for k, v in pools.items() if v["enter"] or v["exit"]}
print(f"\nPools seen: {len(pools)}   pools with an entry/exit: {len(traded)}")
for pool, v in sorted(traded.items(), key=lambda kv: kv[1]["pnl"]):
    print(f"  {pool[:44]:44} {v['mode']:6} entries={v['enter']} exits={v['exit']} pnl={v['pnl'] / 1e9:+.6f} SOL")
for mode, pnl in pnl_by_mode.items():
    label = "simulated" if mode == "paper" else "realized"
    print(f"\nTotal {label} P&L ({mode}): {pnl / 1e9:+.6f} SOL")
