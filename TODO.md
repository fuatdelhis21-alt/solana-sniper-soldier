# TODO — current status

Evidence-based status of every acceptance item: [`docs/READINESS.md`](docs/READINESS.md).
Verdict today: **paper-trading ready, not live-ready.** The live decision is
the operator's; nothing in this repository switches to live by itself.

## Facts the code enforces (keep docs consistent with these)

- Signing: live mode requires the remote HSM over mTLS (`--hsm-endpoint`,
  `--hsm-ca`, `--hsm-client-identity`), `--confirm-live` and
  `SNIPER_EXPECTED_SIGNER`. The HSM server refuses to start without
  `HSM_KEY_B64` — **there is no ephemeral / generated key and no local
  fallback signer**. `--wallet` keyfiles are only used by dry-run.
- Risk limits (`RiskConfig::production_defaults`): 0.05 SOL max per trade,
  5 trades/day, 0.20 SOL daily loss (reaching it, `>=`, stops new entries and
  trips the kill switch), 1 open position, 0.05 SOL total exposure, 2 % max
  slippage, 5 s price staleness (a future timestamp is `clock_skew`). The
  strategy sizes positions at 0.01 SOL (`SNIPER_MAX_SPEND_SOL`).
- Exits are never blocked by the entry limits or the kill switch.
- Quotes: exact output from an on-chain simulation of the swap; integer
  `min_amount_out`; exit/paper P&L and stop-loss / take-profit use the same
  overflow-checked integer price ratio (`strategy::price_ratio_scaled`).

## Open (operator / evidence)

- [ ] Dry-run on a launch pool showing `EXIT simulation as token holder … OK`.
- [ ] Paper sample of ≥ 15–20 closed trades, reviewed with
      `scripts/sniper_report.py` against the checklist in READINESS.md.
- [ ] Latency p50/p95/p99 from the VPS (`sniper_stage_latency_ms`).
- [ ] Load `deploy/prometheus/solana-hft-alerts.yml` and watch both `/ready`.

## Open (code, P2)

- [ ] Solana 3.x / warp 0.4 migration to clear the remaining `cargo audit`
      advisories (pinned by solana-pubsub-client and warp 0.3; see READINESS).
- [ ] Discovery backfill after a WebSocket gap (today: reported via `/ready`).
- [ ] Paper simulation of failed exits.
- [ ] Other DEX adapters (only Raydium CLMM is enabled).

## Done (summary; history in git log)

- Exact on-chain quotes, price-impact and net-edge gates (separate,
  conservative exit estimate), position/liquidity ratio.
- Live entry/exit with balance-based P&L, pending-entry reconciliation,
  persisted position, single-instance lock, fsync'd state writes.
- Remote HSM with mTLS, signature + expected-signer verification, signing
  policy; cert rotation/revocation runbook.
- Signal gate (fail-closed on missing/stale data), paper engine with an
  estimated cost model, pool-discovery supervisor with persisted state.
- Prometheus metrics, `/health` vs `/ready`, alert rules on exported names.
