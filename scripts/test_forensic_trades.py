"""Offline tests for forensic_trades.py (stdlib only)."""
import importlib.util
import os
import unittest

_spec = importlib.util.spec_from_file_location(
    "forensic_trades", os.path.join(os.path.dirname(os.path.abspath(__file__)), "forensic_trades.py"))
ft = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ft)

E = 1 << 64  # sqrt price of 1.0


def rec(t, pool, action, sqrt=E, liq=100_000_000_000, **ctx):
    return {"mode": "paper", "pool_id": pool, "decision_time_ms": t, "sqrt_price": str(sqrt),
            "liquidity": str(liq), "context": dict(action=action, **ctx)}


def sq(move):  # sqrt price for a (1 + move) price ratio
    return int(E * (1 + move) ** 0.5)


class ForensicTests(unittest.TestCase):
    def test_joins_one_trade_per_pool_with_paths(self):
        rs = [
            rec(0, "A", "rejected", liq=90_000_000_000, reject_reason="warming_up"),
            rec(50_000, "A", "enter", signal_confidence=100, entry_sqrt=str(E)),
            rec(51_000, "A", "hold", sqrt=sq(0.04)),
            rec(52_000, "A", "exit_stop_loss", sqrt=sq(-0.07), liq=50_000_000_000,
                net_pnl_lamports=-900_000, mfe_bps=400, mae_bps=-700, hold_ms=2000,
                exit_liquidity_lamports=50_000_000_000),
            rec(0, "B", "rejected", reject_reason="sybil_holders"),
        ]
        ts, unentered = ft.trades(rs, 0)
        self.assertEqual(len(ts), 1)
        t = ts[0]
        self.assertEqual(t["liq_pre_bps"], 1111)      # 90 -> 100 SOL before entry
        self.assertEqual(t["liq_hold_bps"], -5000)    # 100 -> 50 SOL during the hold
        self.assertEqual((t["t_plus3_s"], t["t_minus3_s"]), (1.0, 2.0))
        self.assertEqual(unentered["sybil_holders"], 1)
        out = ft.report(ts, unentered)
        self.assertIn("stop overshoot beyond -500 bps: n=1 median=200", out)
        self.assertIn("losers that were >= +3 % first: 1/1", out)
        self.assertIn("descriptive only", out)

    def test_since_filter_open_positions_and_other_modes(self):
        rs = [rec(10, "A", "enter"), rec(20, "A", "exit_take_profit", net_pnl_lamports=1),
              rec(30, "C", "enter"),  # still open: excluded
              dict(rec(5, "D", "enter"), mode="live")]
        self.assertEqual(len(ft.trades(rs, 0)[0]), 1)
        self.assertEqual(len(ft.trades(rs, 11)[0]), 0)
        self.assertIsNone(ft.ratio_bps("x", "1"))
        self.assertEqual(ft.parse_args(["/d", "--rows"])[0], "/d")


if __name__ == "__main__":
    unittest.main()
