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
# Closed trades in log order: (mode, ts_ms, gross, net or None, cost, hold_ms)
closed = []
quoted_edges = []
latencies = collections.defaultdict(list)
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
        lat = ctx.get("latency_ms") if isinstance(ctx, dict) else None
        if isinstance(lat, dict):
            for stage, ms in lat.items():
                if isinstance(ms, (int, float)):
                    latencies[stage].append(ms)
        if action == "quoted" and isinstance(ctx.get("edge"), dict):
            quoted_edges.append(ctx["edge"])
        if action.startswith("exit_"):
            pnl = ctx.get("realized_simulated_pnl_lamports")
            if pnl is not None:
                cost = ctx.get("cost") or {}
                closed.append((mode, r.get("decision_time_ms", 0), pnl,
                               ctx.get("net_pnl_lamports"),
                               cost.get("total_lamports", 0) if isinstance(cost, dict) else 0,
                               ctx.get("hold_ms")))
        elif action == "exit":
            m = re.search(r"pnl: (-?\d+)", str(ctx.get("outcome", "")))
            pnl = int(m.group(1)) if m else None
            if pnl is not None:
                closed.append((mode, r.get("decision_time_ms", 0), pnl, pnl, 0, None))
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
    label = "simulated gross" if mode == "paper" else "realized"
    print(f"\nTotal {label} P&L ({mode}): {pnl / 1e9:+.6f} SOL")


def pct(values, q):
    v = sorted(values)
    return v[min(len(v) - 1, int(round(q / 100 * (len(v) - 1))))]


def sol(lamports):
    return f"{lamports / 1e9:+.6f} SOL"


for mode in sorted({c[0] for c in closed}):
    trades = [c for c in closed if c[0] == mode]
    # Net when recorded (paper: after the cost estimate; live: realized
    # balance change). Older paper records only have the gross spot move.
    nets = [c[3] if c[3] is not None else c[2] for c in trades]
    gross_only = sum(1 for c in trades if c[3] is None)
    wins = [n for n in nets if n > 0]
    losses = [n for n in nets if n <= 0]
    equity = peak = max_dd = 0
    for n in nets:
        equity += n
        peak = max(peak, equity)
        max_dd = max(max_dd, peak - equity)
    holds = [c[5] for c in trades if isinstance(c[5], (int, float))]
    print(f"\nPerformance ({mode}, {len(trades)} closed trades):")
    print(f"  net P&L:        {sol(sum(nets))}   gross: {sol(sum(c[2] for c in trades))}")
    print(f"  est. costs:     {sol(sum(c[4] for c in trades))}")
    print(f"  win rate:       {len(wins) / len(nets):.0%}")
    print(f"  avg win / loss: {sol(sum(wins) / len(wins)) if wins else '-'} / "
          f"{sol(sum(losses) / len(losses)) if losses else '-'}")
    pf = (sum(wins) / -sum(losses)) if losses and sum(losses) < 0 else None
    print(f"  profit factor:  {pf:.2f}" if pf is not None else "  profit factor:  - (no losses)")
    print(f"  max drawdown:   {sol(-max_dd)}")
    if holds:
        print(f"  hold time:      median {pct(holds, 50) / 1000:.0f} s, max {max(holds) / 1000:.0f} s")
    if gross_only:
        print(f"  note: {gross_only} older record(s) have no cost estimate (gross only)")
    if mode == "paper":
        print("  (paper = estimate: spot fills + modelled fees/impact/latency; failed exits are not simulated)")
    if len(trades) < 15:
        print(f"  sample too small: {len(trades)} < 15 closed trades — no conclusion")

if quoted_edges:
    e = [q.get("net_edge_bps", 0) for q in quoted_edges]
    x = [q.get("exit_cost_bps", 0) for q in quoted_edges]
    print(f"\nExact quotes: {len(e)}  net edge bps p50={pct(e, 50)} min={min(e)}  "
          f"exit cost bps p50={pct(x, 50)}")

if latencies:
    print("\nLatency (ms):")
    for stage, v in sorted(latencies.items()):
        print(f"  {stage:14} n={len(v):5} p50={pct(v, 50):8.0f} p95={pct(v, 95):8.0f} p99={pct(v, 99):8.0f}")
