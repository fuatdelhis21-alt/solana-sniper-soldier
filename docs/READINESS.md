# Readiness — evidence-based status

Verdict: **PAPER-TRADING READY**. Not live-ready: the items marked ❌ or ⚠️
below have no verified evidence yet. Nothing here claims profitability.

Legend: ✅ verified (how is stated) · ⚠️ implemented, evidence incomplete ·
❌ not done / not verified.

## Acceptance criteria

| # | Criterion | Status | Evidence / gap |
|---|---|---|---|
| 1 | Correct quotes for every enabled AMM adapter | ⚠️ | Only Raydium CLMM is enabled. Live/dry-run quotes come from `simulateTransaction` of the exact swap (`sim_quote.rs`): the deployed program computes the output (ticks, fees, impact). Verified on mainnet earlier for the entry swap (PYTH pool; launch pool `AJTe…`, 94k CU) with the previous spot-quote build; **the exact-quote build itself still needs a VPS dry-run** (see below). Paper mode still uses the spot quote (no funded payer) — its P&L is an estimate. |
| 2 | Token decimals and swap direction tested | ✅ | Unit tests: direction from the input vault (`input_is_token0`), held-token price orientation (`signal::held_token_price`), paper exit orientation; quotes are raw-unit integers, decimals not needed. |
| 3 | Quote output and on-chain minimum output consistent | ✅ | `min_amount_out = sim_quote::min_out(exact_out, slippage)` (integer, never 0) is placed in the same instruction that was simulated; tests in `sim_quote.rs`. |
| 4 | Pool identity and account ownership validated | ✅ | `verify_clmm_account` (owner = CLMM program + Anchor discriminator) for PoolState and AmmConfig; tests + mainnet run. |
| 5 | Tick arrays / required accounts resolved | ✅ | Pool bitmap + bitmap extension (`account_resolver.rs`), scan fallback; tests incl. the tick 55 206 / spacing 1 mainnet pool; mainnet entry simulation OK. |
| 6 | Entry and exit paths both work under test | ⚠️ | Unit tests for both builders; HSM policy cross-check on the real entry/exit/tip transactions. Mainnet entry simulation OK; mainnet **exit** simulation only verified on PYTH (payer held PYTH); on a launch pool it needs the token, so it is unverified there. No live round trip has been executed (by design). |
| 7 | Confirmed balance changes drive P&L | ✅ | Live entry/exit use before/after `fetch_mint_balance`; `realized_pnl` tests; pending-entry reconciliation from the chain. |
| 8 | HSM signature and expected signer verified | ✅ | `verify_hsm_signature` against the message bytes, `validate_hsm_pubkey` pin, HSM signing policy, no ephemeral key; tests + local mTLS smoke + CI smoke test. |
| 9 | Mandatory safety checks fail closed | ✅ | Live refuses to start without confirm / HSM / mTLS / expected signer / live risk data (verified by running the release binary); quote simulation failure = no trade; authority/Token-2022/holder/liquidity gates error → no trade. |
| 10 | Paper mode cannot broadcast | ✅ | Paper never builds a transaction; `submit_live_tx` refuses unless live + confirm-live (test `paper_and_dry_run_can_never_broadcast`). |
| 11 | Risk state survives restarts | ✅ | Atomic `risk_state.json`, `live_position.json`, `pending_entry.json` reconciliation; tests. |
| 12 | Discovery handles duplicate / missed events | ⚠️ | Duplicates: signature and pool dedup (tests). Missed events: `logsSubscribe` gives no delivery guarantee; there is **no backfill/reconciliation** (a CLMM-wide `getSignaturesForAddress` scan is too costly). Losses are observable (`pool_sniper_signatures_evicted_total`, `…_unresolved_total`, reconnect counter, stream-age health). |
| 13 | Tests, formatting, compilation, lint pass | ✅ | See the latest change report (fmt, check, test, clippy `-D warnings`, release build). |
| 14 | No unresolved P0/P1 in the enabled live path | ⚠️ | No known open P0. P1 open: exact-quote build not yet run on mainnet; exit simulation on launch pools unverified. |
| 15 | Deployment and rollback documented | ✅ | `README_DEPLOY.md`, `deploy/install_release.sh`, `deploy/rollback.sh`, runbooks. |
| 16 | No unverified claims of profitability / readiness | ✅ | This file; `GO_LIVE_PLAN.md` marked historical. No paper sample with closed trades exists yet. |

## Not implemented (documented, not hidden)

- News / sentiment signals: not implemented (optional in the spec; never a
  trade trigger).
- Multi-DEX: only Raydium CLMM. Other AMMs need their own adapters.
- Latency: no p50/p95/p99 measurements exist yet; none are claimed.
- Dependency audit (`cargo audit`, 2026-10-14): 18 → 10 advisories after
  lockfile patch updates (h2 0.4.20, quinn-proto 0.11.19, rustls 0.23.45,
  rustls-webpki 0.103.15) and prometheus 0.14 without protobuf. The remaining
  10 are pinned by solana-sdk 2.x / solana-pubsub-client and warp 0.3 and need
  a Solana 3.x / warp 0.4 migration:
  `ed25519-dalek 1.0.1` (oracle attack only via mismatched key/pubkey APIs —
  `Keypair` keeps them paired), `curve25519-dalek 3.2` (local timing
  side-channel), `h2 0.3.27` (HTTP/2 DoS; the HSM listens on localhost behind
  mTLS), `rustls-webpki 0.102.8` (HSM server TLS: name-constraint / CRL issues;
  our private CA uses neither) and `0.101.7` (pubsub WebSocket client TLS).
  Tracked as P2.

## Before any live run (operator)

1. VPS dry-run of the exact-quote build on a liquid pool with a funded
   simulation payer: the log must show `exact entry quote (on-chain
   simulation)` with `impact_bps` / `edge_bps`, and `on-chain simulation OK`.
2. Paper run long enough for ≥ 15–20 closed paper trades; review
   `scripts/sniper_report.py` (rejection reasons incl. `signal_*`,
   `price_impact_exceeded`, `net_edge_too_small`).
3. Fund only a small amount (≤ 0.1 SOL), keep the hard caps (0.05 SOL/trade,
   5 trades/day, 0.20 SOL daily loss), watch `/ready` and the alerts; stop on
   any failed exit.
