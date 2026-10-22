"""Offline tests for sniper_report.py (stdlib only).

Run: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sniper_report.py")


def run_report(records, *extra):
    with tempfile.TemporaryDirectory() as d:
        os.makedirs(os.path.join(d, "decisions"))
        with open(os.path.join(d, "decisions", "decisions.jsonl"), "w") as f:
            for r in records:
                f.write((r if isinstance(r, str) else json.dumps(r)) + "\n")
        out = subprocess.run([sys.executable, "-I", SCRIPT, d, *extra],
                             capture_output=True, text=True, check=True)
        return out.stdout


def rec(t, pool, action, mode="paper", **ctx):
    return {"decision_time_ms": t, "mode": mode, "pool_id": pool,
            "context": dict(action=action, **ctx)}


def paper_trade(t, pool, gross, net, cost, size=10_000_000):
    return [
        rec(t, pool, "enter", signal_confidence=80, position_size_lamports=size),
        rec(t + 1, pool, "exit_stop_loss" if gross < 0 else "exit_take_profit",
            realized_simulated_pnl_lamports=gross, net_pnl_lamports=net,
            cost={"total_lamports": cost}, hold_ms=1000, position_size_lamports=size,
            mfe_bps=100, mae_bps=-500),
    ]


class ReportTests(unittest.TestCase):
    def test_gross_net_and_costs_are_counted_once(self):
        out = run_report(paper_trade(1, "A", -500_000, -600_000, 100_000)
                         + paper_trade(10, "B", 1_000_000, 900_000, 100_000))
        self.assertIn("Performance (paper, 2 closed trades)", out)
        self.assertIn("net P&L:        +0.000300 SOL   gross: +0.000500 SOL", out)
        self.assertIn("est. costs:     +0.000200 SOL", out)
        self.assertIn("win rate:       50%", out)
        self.assertIn("profit factor:  1.50", out)

    def test_duplicate_exit_is_counted_once(self):
        trade = paper_trade(1, "A", -500_000, -600_000, 100_000)
        out = run_report(trade + [trade[1]])
        self.assertIn("1 closed trades", out)
        self.assertIn("Duplicate exit records ignored: 1", out)

    def test_open_entry_is_reported_and_excluded(self):
        out = run_report(paper_trade(1, "A", -500_000, -600_000, 100_000)
                         + [rec(50, "C", "enter", position_size_lamports=10_000_000)])
        self.assertIn("1 closed trades", out)
        self.assertIn("Entries without a recorded exit: 1", out)

    def test_stress_case_is_labelled_and_applied_per_exit(self):
        out = run_report(paper_trade(1, "A", -500_000, -600_000, 100_000),
                         "--stress-exit-bps", "200")
        # 200 bps of 0.01 SOL = 200 000 lamports on top of the -600 000 net.
        self.assertIn("stress case:    net -0.000800 SOL", out)
        self.assertIn("assumption, not a prediction", out)

    def test_latency_percentiles_skip_invalid_and_report_missing_stages(self):
        records = [rec(i, "Q", "quoted", mode="dry_run", latency_ms={"quote_sim": i})
                   for i in range(1, 101)]
        records += [rec(200, "Q", "quoted", mode="dry_run",
                        latency_ms={"quote_sim": -5}),
                    rec(201, "Q", "quoted", mode="dry_run",
                        latency_ms={"quote_sim": True}),
                    '{"decision_time_ms": 202, "mode": "dry_run", "pool_id": "Q",'
                    ' "context": {"action": "quoted", "latency_ms": {"quote_sim": NaN}}}']
        out = run_report(records)
        # Nearest rank over 1..100: p50=50, p95=95, p99=99; invalid values
        # are neither counted nor turned into zeros.
        line = next(l for l in out.splitlines() if l.strip().startswith("quote_sim"))
        self.assertEqual(line.split(), ["quote_sim", "n=", "100", "p50=", "50",
                                        "p95=", "95", "p99=", "99"])
        self.assertIn("ignored invalid durations: 3", out)
        self.assertIn("exit           no samples", out)

    def test_single_sample_and_small_sample_warning(self):
        out = run_report(paper_trade(1, "A", 1_000_000, 900_000, 100_000))
        self.assertIn("sample too small: 1 < 15 closed trades", out)
        self.assertIn("profit factor:  - (no losses)", out)


if __name__ == "__main__":
    unittest.main()
