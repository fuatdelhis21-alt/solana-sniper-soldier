# Readiness — evidence-based status

Verdict: **PAPER-TRADING READY — NOT LIVE-READY.** Nothing here claims
profitability. The live decision is the operator's; nothing in this
repository switches to live by itself.

Evidence kinds: **U** unit test (offline, runs in CI) · **L** local
integration (run in the dev container, no network) · **O** operator/VPS
output pasted by the operator · **M** mainnet observation (operator) ·
**—** not verified.

## Acceptance criteria

| # | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | Correct quotes for the enabled AMM (Raydium CLMM only) | ✅ liquid pool / ⚠️ launch pools | **U** exact quote = `simulateTransaction` of the real swap; mocked simulations: success, no output, program error, missing state, non-token data all reject (`sim_quote` tests). **M** 2026-10-14 PYTH/wSOL dry-run: spot 13218796 / exact 13218790, impact 0, entry + exit simulation OK. **—** never observed on a thin launch pool. |
| 2 | Decimals and swap direction | ✅ | **U** direction from the input vault, held-token price orientation, paper exit orientation; raw-unit integer quotes. |
| 3 | Quote ↔ on-chain minimum output | ✅ | **U** `min_out` integer, never 0, placed in the simulated instruction. |
| 4 | Pool identity, ownership, mint, token program | ✅ | **U** `verify_clmm_account` (owner + discriminator, fail-closed); `classify_mint`: live/dry-run accept only an initialized 82-byte classic SPL mint (Token-2022, other owners, malformed rejected). Paper with the opt-in `--paper-token-2022` also accepts a Token-2022 mint whose only extensions are metadata pointer + metadata; every other or unknown extension and malformed TLV/padding/account type reject (**U**); both binaries refuse the option outside paper (**U**). **—** no Token-2022 pool observed on the VPS yet. Vaults selected by mint. |
| 5 | Tick arrays / required accounts | ✅ | **U** bitmap + extension resolver incl. a mainnet layout, uninitialized arrays skipped, missing extension fail-closed. **M** PYTH entry simulation. |
| 6 | Entry and exit paths work | ⚠️ | **U** both builders; HSM policy on real entry/exit/tip txs; holder selection for the launch-pool exit simulation (on-curve owner, ATA, balance). **M** exit simulation on PYTH. **—** launch-pool exit simulation as a token holder has never been run on the VPS. No live round trip (by design). |
| 7 | Confirmed balance changes drive P&L; ambiguous outcomes | ✅ | **U** pending-entry reconciliation: landed, never landed, blockhash expired, outcome unknown → kill switch; corrupt pending file refuses live. |
| 8 | HSM signature and expected signer | ✅ | **U** signature / signer / fee-payer verification, policy, no ephemeral key, no fallback signer. **L** CI mTLS smoke test. Rotation/revocation: `HSM_MTLS_OPS.md` §5. |
| 9 | Safety checks fail closed | ✅ | **U** stale / future (`clock_skew`) / invalid signal data, unknown liquidity, position vs liquidity, impact cap, net edge, daily loss at `>=`, unpersistable risk state blocks entries, `sybil_holders` (≥ 5 sampled top holders with the very same balance). **O** 2026-11-06 the holder gate passed PumpSwap pool `FudwRS…` (token `ZajZXL…pump`): ~2 000 wallets held exactly 478 536 tokens each, so the top-20 showed ~1 % of supply; 5 s after the paper entry they were swept into one wallet (98 % of supply), which sold and drained 936 of the pool's 941 SOL (paper loss −99.96 %, −0.0105 SOL). The `sybil_holders` gate rejects that distribution (**U**, regression test with those amounts). |
| 10 | Paper / dry-run never broadcast | ✅ | **U** `paper_and_dry_run_can_never_broadcast`; paper builds no transaction. |
| 11 | Restart safety; one live process | ✅ | **U** fsync'd atomic state files, corrupt state refuses live, `InstanceLock`, discovery state persisted. |
| 12 | Discovery: duplicates, malformed events, gaps | ⚠️ | **U** bounded dedup, persisted traded pools + daily cap, bounded channel/queue, malformed CreatePool (wrong program, short accounts, bad indexes) ignored, `/ready` gap window, stream-down ERROR alarm. **O** 2026-10-16/17: the WS provider answered HTTP 429 for ~17 h; discovery stopped (fail-closed, no trades) — it was found only by a manual check. **Gap:** no backfill of missed pools (a CLMM-wide signature scan is too costly); missed pools stay missed. **O** 2026-10-22…26: QuickNode's free plan hit its daily request limit (`-32003`) with the `logs` source; Chainstack's free plan refuses `getProgramAccounts`. **U** opt-in `geckoterminal` source: listing parser (other DEXes, too-old and malformed entries dropped), bounded dedup, page-coverage gap detection, every listed pool verified on-chain (owner + discriminator) before it is watched. **—** not yet run on the VPS. |
| 13 | Exits never blocked by entry limits | ✅ | **U** exit attempted with the kill switch active; failed exit keeps the position; `exit_blocked` after 3 consecutive failures (alert, live `/ready` 503), retries continue. |
| 14 | Realistic net edge | ⚠️ | **U** `edge_breakdown`: both fees, measured entry impact, exit impact estimated (×200 %), network fees, failed-tx reserve, slippage as worst case. The exit leg is an estimate (it cannot be simulated before holding the tokens). |
| 15 | Realistic paper engine | ⚠️ | **U** gross and net once per close, costs = sum of parts, nothing booked after a close, integer P&L shared with the exit decision, MFE/MAE per trade; report: duplicates ignored, open entries excluded, labelled exit stress case. Still an estimate: spot fills, failed exits/MEV only as a stress assumption, an open paper position is lost on restart (reported as an entry without exit). **O** 72 h sample: 4 closed trades (1 TP, 3 SL), net −0.000997 SOL — far too small for any conclusion. Exits are checked every `SNIPER_POSITION_POLL_MS` (default 1 s) while a position is open instead of at the loop interval (**U**); **O** 2026-11-07…09 with 5 s polling, two −5 % stops filled at −16 % and −23 %. |
| 16 | Latency measured | ⚠️ | **U** `sniper_stage_latency_ms{stage}` (monotonic `Instant`, fixed stage set, ms buckets); report: nearest-rank p50/p95/p99, invalid durations ignored, missing stages shown as "no samples". **—** no VPS measurements yet. |
| 17 | RPC load bounded | ⚠️ | **U** paper reuses the pool's identity accounts for 300 s; revoked mints are not re-read. Estimate by code inspection: ~2 RPC calls per paper iteration instead of ~7–8. **—** not measured on the VPS. |
| 18 | Tests, formatting, lint, build | ✅ | **L** see the latest change report (fmt, test, clippy `--all-features -D warnings`, release build, report tests, `bash -n`). CI runs the same. |
| 19 | No unresolved P0/P1 in the enabled live path | ⚠️ | No known P0. Open P1: launch-pool exit simulation not observed; paper sample < 15; no latency data; RPC provider quota (operator). |
| 20 | Deployment, alerts, rollback | ✅ | `README_DEPLOY.md`, install/rollback scripts with `REVISION`, `VPS_RUNBOOK.md` §9, alert rules on exported metric names. **—** alert rules not confirmed loaded on the VPS. |
| 21 | No unverified profitability / readiness claims | ✅ | This file; signal confidence is a score, not a probability. |

## Paper acceptance checklist (before considering live)

Run paper until **15–20 closed trades** exist, then
`python3 /opt/solana-hft/sniper_report.py /var/lib/solana-hft`. 15–20
trades is a smoke test of the pipeline, **not statistical evidence of an
edge**.

- [ ] ≥ 15 closed paper trades with `net_pnl_lamports`.
- [ ] Net P&L (not gross) ≥ 0, profit factor ≥ 1.2, and the stress case reviewed.
- [ ] Max drawdown ≤ the 0.20 SOL daily loss limit scaled to 0.05 SOL trades.
- [ ] Average loss not larger than ~1.5× the stop-loss (5 %).
- [ ] MFE/MAE reviewed: losers that were first in profit (exit problem) vs straight down (entry problem).
- [ ] No unexplained `data_error` / `quote_simulation_failed` / `clock_skew` spikes.
- [ ] Higher signal-confidence buckets not worse than lower ones.
- [ ] A dry-run on a launch pool printed `EXIT simulation as token holder … OK`.
- [ ] Latency p95 of `quote_sim` and `sign` recorded.
- [ ] Both `/ready` endpoints green for 24 h; no `CLMM log stream DOWN` errors.

## Dependency audit (`cargo audit`, 2026-10-22)

18 vulnerabilities + 10 warnings, none suppressed. This change updated
rustls-webpki 0.103.16 and webpki-roots / webpki-root-certs 1.0.10 (21 → 18).
Remaining, by dependency path — a fix needs a Solana 3.x / warp 0.4 migration:

| Crate | Path | Exposure / mitigation |
|---|---|---|
| ed25519-dalek 1.0.1, curve25519-dalek 3.2 | solana-sdk 2.x | Oracle attack needs mismatched key/pubkey APIs (`Keypair` keeps them paired); timing side channel is local. |
| rustls 0.21.12, rustls-webpki 0.101.7, webpki-roots 0.24 / 0.25 | solana-pubsub-client 2.3 (WebSocket to the RPC provider) | Name constraints / CRL issues are not used by public WebPKI certs; the distrusted emSign roots would need a mis-issued cert for the provider's host. `Debug` of client sessions is never logged. |
| rustls 0.22.4, rustls-webpki 0.102.8, h2 0.3.27 | warp 0.3 (HSM server) | Listens on 127.0.0.1 only, mTLS with a private CA (no name constraints / CRLs). |
| unmaintained / unsound: atty, bincode, derivative, libsecp256k1, number_prefix, paste, rustls-pemfile, memmap2, rand 0.7 | solana / warp dependency trees | No direct use. |

## Not implemented (documented, not hidden)

- Discovery backfill after a stream gap; news/sentiment.
- Other DEXes. PumpSwap (2026-11 survey: ~19× more liquid, mint-clean new SOL
  pools per day than Raydium CLMM) is in progress, paper only:
  `amm/pumpswap.rs` decodes `Pool` / pump-fees `FeeConfig` and quotes
  exact-in buys and sells, matching the official SDK's mainnet goldens (**U**).
  The paper engine runs on a PumpSwap pool with `--amm pumpswap` (pool, both
  vaults, base mint and fee config read together and checked; price = reserve
  ratio; liquidity = withdrawable SOL; fee = full LP + protocol + creator
  rate; same token gates) (**U**). Only canonical pump.fun pools (pump
  `pool-authority` creator, token/wSOL) are accepted (**U**). Discovery:
  `SNIPER_DISCOVERY_AMM=pumpswap` with the GeckoTerminal source, paper only
  (**U**). **O** 2026-11-04 manual paper run on two live pools: all accounts
  read and verified; one rejected by the holder gate (top-20 76 %), one
  rejected as a reversed (non-canonical) pool. Dry-run (simulation only,
  `--simulate-payer`, never sent): the SDK's `buy` / `sell` instructions
  (IDL account order, remaining accounts, discriminators checked against
  sha256) simulated for the entry and, as a real holder, the exit; model vs
  simulated amounts and quote latency recorded (**U**). **O** 2026-11-06
  mainnet dry-run on canonical pool `5Fhn…oy3` (0.01 SOL): buy 271 355 920
  tokens model = simulated, 9 999 999 lamports spent (0 bps); sell as a
  real holder 9 860 724 lamports model = simulated (0 bps); 116 559 / 85 919
  CU; fee tier 70 bps. The paper fee + impact model is exact; the 50 bps
  per-leg latency allowance (price drift, MEV) cannot be seen by a
  simulation and stays an assumption. No live path.
- Paper simulation of failed exits / MEV beyond the labelled stress case.
- Persistence of an open **paper** position across restarts.

## Before any live run (operator)

1. RPC/WS provider with enough quota; no `429` (see `VPS_RUNBOOK.md` §9).
2. Dry-run on a liquid pool **and** a launch pool: `exact entry quote` with
   `edge_bps` / `exit_cost_bps`, and an OK exit simulation (payer or holder).
3. Paper acceptance checklist above.
4. ≤ 0.1 SOL, hard caps unchanged, alert rules loaded, `/ready` watched;
   stop on any `SolanaHftExitBlocked`.
