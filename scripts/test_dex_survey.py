"""Offline tests for dex_survey.py (stdlib only).

Run: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import importlib.util
import json
import os
import unittest

_spec = importlib.util.spec_from_file_location(
    "dex_survey", os.path.join(os.path.dirname(os.path.abspath(__file__)), "dex_survey.py"))
ds = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ds)

TOKEN = "Mint111111111111111111111111111111111111111"


def item(addr, dex, base, quote, created="2026-11-02T10:00:00Z", reserve="20000",
         qprice="200", bprice="0.001"):
    return {"attributes": {"address": addr, "pool_created_at": created,
                           "reserve_in_usd": reserve, "quote_token_price_usd": qprice,
                           "base_token_price_usd": bprice},
            "relationships": {"dex": {"data": {"id": dex}},
                              "base_token": {"data": {"id": "solana_" + base}},
                              "quote_token": {"data": {"id": "solana_" + quote}}}}


def mint(mint_auth=False, freeze=False, size=82):
    d = bytearray(size)
    d[45] = 1
    if mint_auth:
        d[0] = 1
    if freeze:
        d[46] = 1
    return d


def t22(exts, base=None):
    d = bytearray(base or mint())
    d.extend(bytes(165 - len(d)))
    d.append(1)
    for ty, ln in exts:
        d += ty.to_bytes(2, "little") + ln.to_bytes(2, "little") + bytes([0xAB]) * ln
    return bytes(d)


class ListingTests(unittest.TestCase):
    def test_sol_pairs_mint_side_and_sol_price(self):
        body = json.dumps({"data": [
            item("P1", "pumpswap", TOKEN, ds.WSOL),
            item("P2", "raydium", ds.WSOL, TOKEN, bprice="190"),
            item("P3", "meteora", TOKEN, "USDC111"),
            {"attributes": {"address": "X"}},
        ]})
        pools, bad = ds.parse_listing(body)
        self.assertEqual(bad, 1)
        self.assertEqual([p["mint"] for p in pools], [TOKEN, TOKEN, None])
        self.assertEqual([p["sol_price"] for p in pools], [200.0, 190.0, None])
        self.assertEqual(pools[0]["dex"], "pumpswap")

    def test_non_listing_is_an_error(self):
        for body in ['{"errors":[{"status":"429"}]}', '[]']:
            with self.assertRaises(ValueError):
                ds.parse_listing(body)
        with self.assertRaises(ValueError):
            ds.parse_listing("<html>")

    def test_rate_limit_detection(self):
        import urllib.error
        e = urllib.error.HTTPError("u", 429, "Too Many Requests", {}, None)
        self.assertTrue(ds.is_rate_limited(e))
        self.assertTrue(ds.is_rate_limited(ValueError("HTTP Error 429")))
        self.assertFalse(ds.is_rate_limited(OSError("timed out")))

    def test_due_lookups_are_bounded_and_backlog_expires(self):
        m = 60_000
        tracked = {f"P{i}": {"pool": f"P{i}", "dex": "d", "first_seen": t}
                   for i, t in enumerate([0, 10 * m, 25 * m, 50 * m, 55 * m])}
        due, expired = ds.pick_due(tracked, now=70 * m, check_ms=30 * m, limit=1)
        self.assertEqual([p["pool"] for p in expired], ["P0"])  # waited 70 > 60 min
        self.assertEqual([p["pool"] for p in due], ["P1"])      # oldest first, limit 1
        due, _ = ds.pick_due(tracked, now=70 * m, check_ms=30 * m, limit=10)
        self.assertEqual([p["pool"] for p in due], ["P1", "P2"])  # P3/P4 not due yet

    def test_sol_estimate(self):
        self.assertEqual(ds.sol_side_estimate(20000, 200), 50)
        self.assertIsNone(ds.sol_side_estimate(None, 200))
        self.assertIsNone(ds.sol_side_estimate(20000, None))
        self.assertIsNone(ds.to_float("nan"))


class MintTests(unittest.TestCase):
    def test_classic(self):
        self.assertEqual(ds.classify_mint(ds.SPL_TOKEN, mint()), ("ok_classic", ""))
        self.assertEqual(ds.classify_mint(ds.SPL_TOKEN, mint(freeze=True))[0], "authority")
        self.assertEqual(ds.classify_mint(ds.SPL_TOKEN, mint(size=90))[0], "malformed")
        self.assertEqual(ds.classify_mint("Other", mint())[0], "other_program")

    def test_token_2022_matches_the_bot_allow_list(self):
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, mint())[0], "ok_token2022")
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, t22([(18, 64), (19, 90)]))[0],
                         "ok_token2022")
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, t22([(18, 64), (12, 32)])),
                         ("bad_extension", "PermanentDelegate"))
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, t22([(99, 4)])),
                         ("bad_extension", "unknown(99)"))
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, t22([(19, 8)], mint(True)))[0],
                         "authority")
        bad = bytearray(t22([(19, 8)]))
        bad[100] = 1
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, bytes(bad))[0], "malformed")
        self.assertEqual(ds.classify_mint(ds.TOKEN_2022, t22([(19, 8)]) + b"\x12\x00")[0],
                         "malformed")


class SummaryTests(unittest.TestCase):
    def test_counts_per_dex(self):
        recs = [
            {"kind": "start", "min_sol": 50, "check_after_min": 30, "rpc": True},
            {"kind": "new", "dex": "pumpswap", "sol_pair": True},
            {"kind": "new", "dex": "pumpswap", "sol_pair": True},
            {"kind": "new", "dex": "meteora", "sol_pair": False},
            {"kind": "checked", "dex": "pumpswap", "liquid": True, "verdict": "ok_token2022"},
            {"kind": "checked", "dex": "pumpswap", "liquid": False},
            {"kind": "gap"}, {"kind": "gecko_error"}, {"kind": "unchecked", "dex": "x"},
        ]
        out = ds.summarize(recs)
        row = next(l for l in out.splitlines() if l.startswith("pumpswap"))
        self.assertEqual(row.split()[1:], ["2", "2", "2", "1", "0", "1", "0", "0", "0"])
        self.assertIn("listing gaps (pools may have been missed): 1; GeckoTerminal errors: 1; "
                      "pools never checked (rate limit backlog): 1", out)
        self.assertIn("NOT measured", out)

    def test_redact_hides_rpc_url_and_host(self):
        url = "https://secret-host.example/abc123key/"
        msg = ds.redact(f"<urlopen error {url}> at secret-host.example", url)
        self.assertNotIn("abc123key", msg)
        self.assertNotIn("secret-host", msg)


if __name__ == "__main__":
    unittest.main()
