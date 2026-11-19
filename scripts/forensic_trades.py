#!/usr/bin/env python3
"""Per-trade forensic view of paper decision records (read-only, stdlib only).

  python3 scripts/forensic_trades.py [DATA_DIR] [--since 2026-11-09T06:47:31Z]

Reads DATA_DIR/decisions/decisions.jsonl (default /var/lib/solana-hft) and,
for every paper trade whose entry is at or after --since, joins the records
of that pool (one trading process trades one pool once):

- liquidity at entry, its change over the 60 s before the entry and over
  the hold (from the `liquidity` field every post-snapshot record carries);
- the price path after the entry (from `sqrt_price`, ratio = (cur/entry)^2):
  MFE / MAE at the recorded samples and the time to reach +3 % and -3 %;
- the stop-loss overshoot beyond the configured -5 % stop;
- signal confidence, entry hour (UTC), hold time, net P&L.

It then compares winners and losers on each feature (medians, n). With the
sample sizes this project has, the output describes the sample; it is not
evidence of a predictive relationship. Nothing is written anywhere.
"""
import collections
import datetime
import json
import statistics
import sys

STOP_BPS = 500
PRE_ENTRY_MS = 60_000


def parse_args(argv):
    data_dir, since_ms = "/var/lib/solana-hft", 0
    args = [a for a in argv if a != "--rows"]
    if "--since" in args:
        i = args.index("--since")
        stamp = args[i + 1].replace("Z", "+00:00")
        since_ms = int(datetime.datetime.fromisoformat(stamp).timestamp() * 1000)
        del args[i:i + 2]
    if args:
        data_dir = args[0]
    return data_dir, since_ms


def ratio_bps(entry_sqrt, cur_sqrt):
    """Signed price move in bps from Q64.64 sqrt prices; None if unusable."""
    try:
        e, c = int(entry_sqrt), int(cur_sqrt)
    except (TypeError, ValueError):
        return None
    if e <= 0 or c <= 0:
        return None
    return round(((c / e) ** 2 - 1) * 10_000)


def as_int(v):
    try:
        return int(v)
    except (TypeError, ValueError):
        return None


def trades(records, since_ms):
    """Group paper records by pool into closed trades (pure, testable)."""
    by_pool = collections.defaultdict(list)
    for r in records:
        if r.get("mode") == "paper":
            by_pool[r.get("pool_id", "")].append(r)
    out, unentered = [], collections.Counter()
    for pool, rs in by_pool.items():
        rs.sort(key=lambda r: r.get("decision_time_ms", 0))
        actions = [(r.get("context") or {}).get("action") for r in rs]
        if "enter" not in actions:
            last = next((a for a in reversed(actions) if a), "none")
            reason = (rs[-1].get("context") or {}).get("reject_reason") or last
            unentered[reason] += 1
            continue
        ei = actions.index("enter")
        xi = next((i for i in range(ei + 1, len(rs)) if str(actions[i]).startswith("exit_")), None)
        enter = rs[ei]
        t0 = enter.get("decision_time_ms", 0)
        if xi is None or t0 < since_ms:
            continue
        exit_ = rs[xi]
        ctx = exit_.get("context") or {}
        entry_sqrt = (enter.get("context") or {}).get("entry_sqrt") or enter.get("sqrt_price")
        liq0 = as_int(enter.get("liquidity"))
        pre = [as_int(r.get("liquidity")) for r in rs[:ei]
               if t0 - PRE_ENTRY_MS <= r.get("decision_time_ms", 0) < t0]
        pre = [x for x in pre if x]
        path = []
        for r in rs[ei + 1:xi + 1]:
            m = ratio_bps(entry_sqrt, r.get("sqrt_price") or (r.get("context") or {}).get("current_sqrt"))
            if m is not None:
                path.append((r.get("decision_time_ms", 0) - t0, m))
        up3 = next((dt for dt, m in path if m >= 300), None)
        down3 = next((dt for dt, m in path if m <= -300), None)
        liq_exit = as_int(ctx.get("exit_liquidity_lamports")) or as_int(exit_.get("liquidity"))
        out.append({
            "pool": pool,
            "action": ctx.get("action"),
            "net": ctx.get("net_pnl_lamports") if ctx.get("net_pnl_lamports") is not None
            else ctx.get("realized_simulated_pnl_lamports"),
            "confidence": (enter.get("context") or {}).get("signal_confidence"),
            "hour_utc": datetime.datetime.fromtimestamp(t0 / 1000, datetime.timezone.utc).hour,
            "hold_s": (ctx.get("hold_ms") or 0) / 1000,
            "liq_entry_sol": liq0 / 1e9 if liq0 else None,
            "liq_pre_bps": round((liq0 - pre[0]) * 10_000 / pre[0]) if liq0 and pre else None,
            "liq_hold_bps": round((liq_exit - liq0) * 10_000 / liq0) if liq0 and liq_exit else None,
            "mfe_bps": ctx.get("mfe_bps"),
            "mae_bps": ctx.get("mae_bps"),
            "t_plus3_s": up3 / 1000 if up3 is not None else None,
            "t_minus3_s": down3 / 1000 if down3 is not None else None,
            "samples": len(path),
        })
    return out, unentered


def median(v):
    v = [x for x in v if isinstance(x, (int, float))]
    return (statistics.median(v), len(v)) if v else (None, 0)


def report(ts, unentered):
    lines = [f"closed paper trades: {len(ts)}"]
    if unentered:
        lines.append("pools with records but no entry, by last reason: "
                     + ", ".join(f"{k}={v}" for k, v in unentered.most_common(8)))
    wins = [t for t in ts if (t["net"] or 0) > 0]
    loss = [t for t in ts if (t["net"] or 0) <= 0]
    lines.append(f"winners {len(wins)}, losers {len(loss)}  (median, n)")
    for f in ("liq_entry_sol", "liq_pre_bps", "liq_hold_bps", "confidence", "hour_utc",
              "hold_s", "mfe_bps", "mae_bps", "t_plus3_s", "t_minus3_s", "samples"):
        w, l = median(t[f] for t in wins), median(t[f] for t in loss)
        lines.append(f"  {f:14} winners {w[0]!s:>10} (n={w[1]:3})   losers {l[0]!s:>10} (n={l[1]:3})")
    stops = [t for t in ts if t["action"] == "exit_stop_loss" and isinstance(t["mae_bps"], int)]
    if stops:
        over = sorted(-t["mae_bps"] - STOP_BPS for t in stops)
        lines.append(f"stop overshoot beyond -{STOP_BPS} bps: n={len(over)} median={statistics.median(over):.0f} "
                     f"max={over[-1]}  values={over}")
    up_first = sum(1 for t in loss if isinstance(t["mfe_bps"], int) and t["mfe_bps"] >= 300)
    if loss:
        lines.append(f"losers that were >= +3 % first: {up_first}/{len(loss)}")
    lines.append("descriptive only: small samples, no significance test, no causal claim")
    return "\n".join(lines)


def main(argv):
    data_dir, since_ms = parse_args(argv)
    path = f"{data_dir}/decisions/decisions.jsonl"
    records = []
    with open(path) as fh:
        for line in fh:
            try:
                records.append(json.loads(line))
            except ValueError:
                continue
    ts, unentered = trades(records, since_ms)
    print(report(ts, unentered))
    if "--rows" in argv:
        for t in sorted(ts, key=lambda t: t["net"] or 0):
            print(json.dumps(t, sort_keys=True))


if __name__ == "__main__":
    main(sys.argv[1:])
