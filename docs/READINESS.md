# Readiness — evidence-based status

Verdict: **PAPER-TRADING READY**. Not live-ready: the items marked ❌ or ⚠️
below have no verified evidence yet. Nothing here claims profitability.
The live decision is the operator's; nothing in this repository switches
to live by itself.

Legend: ✅ verified (how is stated) · ⚠️ implemented, evidence incomplete ·
❌ not done / not verified. "Unit test" = runs in CI (`cargo test`), no
network; "mainnet" = observed on the VPS by the operator.

## Acceptance criteria

| # | Criterion | Status | Evidence / gap |
|---|---|---|---|
| 1 | Correct quotes for every enabled AMM adapter | ✅ (liquid pool) / ⚠️ (launch pools) | Only Raydium CLMM is enabled. Live/dry-run quotes come from `simulateTransaction` of the exact swap (`sim_quote.rs`); simulation errors / missing state fail closed (unit test). **Mainnet 2026-10-14** (PYTH/wSOL, 0.01 SOL): `spot_out 13218796`, `exact_out 13218790`, impact 0, entry + exit simulations OK. Not yet observed on a thin launch pool. |
| 2 | Token decimals and swap direction tested | ✅ | Unit tests: direction from the input vault, held-token price orientation, paper exit orientation; raw-unit integer quotes. |
| 3 | Quote output and on-chain minimum output consistent | ✅ | `min_amount_out = min_out(exact_out, slippage)` (integer, never 0) in the very instruction that was simulated (unit tests). |
| 4 | Pool identity, ownership, mint, token program validated | ✅ | `verify_clmm_account` (owner + Anchor discriminator). `classify_mint` (pure, unit test): only an initialized 82-byte classic SPL mint with well-formed authority options; Token-2022 / any other owner rejected; any authority present rejected. |
| 5 | Tick arrays / required accounts resolved | ✅ | Bitmap + extension resolver (unit tests incl. a mainnet layout); mainnet entry simulation OK. A missing / uninitialized array makes the simulation fail → no trade. |
| 6 | Entry and exit paths both work | ⚠️ | Unit tests for both builders + HSM policy on real entry/exit/tip txs. Mainnet exit simulation verified on PYTH. **New:** dry-run also simulates the exit as an existing on-curve token holder (`find_simulation_holder`), so the exit path can be checked on a launch pool without holding the token — **not yet run on the VPS**. No live round trip (by design). |
| 7 | Confirmed balance changes drive P&L | ✅ | Before/after `fetch_mint_balance`; `realized_pnl` tests; pending-entry reconciliation from the chain (tests: landed, never landed, blockhash expired, unknown). |
| 8 | HSM signature and expected signer verified | ✅ | `verify_hsm_signature`, pubkey pin, signing policy, no ephemeral key, no fallback signer; tests + CI mTLS smoke test. Cert rotation/revocation procedure: `HSM_MTLS_OPS.md` §5. |
| 9 | Mandatory safety checks fail closed | ✅ | Unit tests: invalid/stale/future/zero-liquidity signal data → NoTrade; unknown liquidity, position > `SNIPER_MAX_POSITION_LIQUIDITY_BPS`, impact > cap, net edge < min → reject; unknown pool fee = 1% tier; a failed risk-state write blocks new entries. |
| 10 | Paper mode cannot broadcast | ✅ | Paper never builds a transaction; `submit_live_tx` refuses unless live + confirm-live (test `paper_and_dry_run_can_never_broadcast`). |
| 11 | Risk state survives restarts; one live process | ✅ | fsync'd atomic writes (`write_atomic`) for `risk_state.json`, `live_position.json`, `pending_entry.json`, `discovery_state.json`; corrupt state refuses live; `InstanceLock` (OS lock, unit test) — a second live process on the same data dir refuses to start. |
| 12 | Discovery handles duplicate / missed events | ⚠️ | Duplicates: bounded dedup + persisted traded pools and daily cap (unit tests). Queues bounded (channel 1024, pending list). Missed events: **no backfill** (`logsSubscribe` has no replay; a CLMM-wide signature scan is too costly). Possible gaps are surfaced: pool-sniper `/ready` 503 for 5 min after a reconnect / overflow / unresolved CreatePool, `pool_sniper_last_gap_age_ms`, alerts. |
| 13 | Exits are never blocked by entry limits | ✅ | The exit branch runs before every entry gate; unit test: with the kill switch active, the exit is still attempted and a failed exit keeps `live_position.json`. ≥ 3 consecutive failures → `exit_blocked` (live `/ready` 503, critical alert); retries continue. |
| 14 | Realistic net edge | ⚠️ | `edge_breakdown` (unit tests, i128/saturating): both legs' fees, measured entry impact, **exit impact estimated** at `SNIPER_EXIT_IMPACT_PCT` (default 200 %) of the entry impact, network fees per tx, a failed-tx reserve; slippage reported separately (worst case). Logged and recorded as `"action":"quoted"`. The exit is an estimate — it cannot be simulated before the tokens are held. ATA rent is refunded on a full exit and not counted. |
| 15 | Realistic paper engine | ⚠️ | Paper P&L is now **gross (spot move) and net** after an estimated cost model (fees both legs, constant-product impact with exit at exit-time liquidity × exit multiplier, `SNIPER_PAPER_LATENCY_BPS` per leg, network fees), unit tested. Still an estimate: fills are at the observed spot, and **failed exits are not simulated**. Existing VPS sample: 1 closed paper trade (gross +0.001099 SOL) — far below 15. |
| 16 | Latency measured | ⚠️ | `sniper_stage_latency_ms{stage}` (market_data, quote_sim, sign, submit_confirm, exit; fixed label set) and ms buckets for the HSM/trade histograms; `latency_ms` in records; report prints p50/p95/p99. **No VPS measurements yet.** |
| 17 | Tests, formatting, compilation, lint pass | ✅ | Latest change report: fmt, `cargo test --workspace`, clippy `-D warnings`, release build, `bash -n` on deploy scripts. |
| 18 | No unresolved P0/P1 in the enabled live path | ⚠️ | No known open P0. Open P1: launch-pool exit simulation not yet observed on the VPS; paper sample < 15 closed trades; no latency data. |
| 19 | Deployment, alerts and rollback documented | ✅ | `README_DEPLOY.md`, `deploy/install_release.sh`, `deploy/rollback.sh`, `VPS_RUNBOOK.md` §9 (exit failure, kill switch, state, lock, discovery gap). Alert rules now only use exported metric names (previous rules referenced metrics that did not exist). |
| 20 | No unverified claims of profitability / readiness | ✅ | This file. The signal confidence is a score, not a probability; outcomes are grouped by it in the report so it can be checked. |

## Paper acceptance checklist (before considering live)

Run paper (default) until **15–20 closed trades** exist, then
`python3 /opt/solana-hft/sniper_report.py /var/lib/solana-hft`. 15–20
trades is a smoke test of the pipeline, **not statistical evidence of an
edge**; a profitable sample of this size can be luck.

- [ ] ≥ 15 closed paper trades, all with `net_pnl_lamports` (post-cost records).
- [ ] Net P&L (not gross) ≥ 0 and profit factor ≥ 1.2.
- [ ] Max drawdown ≤ the 0.20 SOL daily loss limit scaled to 0.05 SOL trades.
- [ ] Average loss not larger than ~1.5× the stop-loss (5 %) — larger means exits fill far from the trigger.
- [ ] Median hold time consistent with `SNIPER_DISCOVERY_MAX_HOLD_SECS`.
- [ ] No unexplained `data_error` / `quote_simulation_failed` spikes; rejection reasons reviewed.
- [ ] Higher signal-confidence buckets not worse than lower ones (otherwise the score adds nothing).
- [ ] At least one dry-run on a launch pool printed `EXIT simulation as token holder … OK`.
- [ ] Latency p95 of `quote_sim` and `sign` recorded (report / `/metrics`).
- [ ] `/ready` of both services green for 24 h; no `PoolSniperNotReady` storms.

## Not implemented (documented, not hidden)

- News / sentiment signals (optional in the spec; never a trade trigger).
- Multi-DEX: only Raydium CLMM.
- Discovery backfill after a stream gap (see #12).
- Paper simulation of failed exits / MEV / adverse selection beyond the
  fixed latency penalty.
- Dependency audit (`cargo audit`, 2026-10-15): 14 vulnerabilities +
  11 warnings, none suppressed. Lockfile updates in this change: rustls
  0.23.45 → 0.23.46, chacha20 0.10.1 (yanked) → 0.10.2. Remaining, pinned by
  solana 2.x / solana-pubsub-client (tokio-tungstenite 0.20 → rustls 0.21,
  rustls-webpki 0.101) and warp 0.3 (rustls 0.22, rustls-webpki 0.102, h2 0.3):
  `ed25519-dalek 1.0.1`, `curve25519-dalek 3.2`, `h2 0.3.27`, `rustls 0.21.12`
  / `0.22.4` (RUSTSEC-2026-0346 Debug impls expose session secrets — we never
  Debug-format TLS sessions; -0349 hybrid key-exchange downgrade — these
  versions are classical-only anyway), `rustls-webpki 0.101.7 / 0.102.8`
  (name constraints / CRL parsing — the private HSM CA uses neither; the
  pubsub client talks to the RPC provider's public certificate). Fix needs a
  Solana 3.x / warp 0.4 migration. Tracked as P2.

## Before any live run (operator)

1. Dry-run of this build on a liquid pool **and** on a launch pool: the log
   must show `exact entry quote (on-chain simulation)` with `edge_bps`,
   `exit_cost_bps`, and an OK exit simulation (payer or token holder).
2. Complete the paper acceptance checklist above.
3. Fund only a small amount (≤ 0.1 SOL), keep the hard caps (0.05 SOL/trade,
   5 trades/day, 0.20 SOL daily loss, 1 open position), load the alert rules,
   watch `/ready`; stop on any `SolanaHftExitBlocked`.
