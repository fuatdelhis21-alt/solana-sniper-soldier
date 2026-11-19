# Strategy forensic audit — Solana Sniper (TASK 001)

Date: 2026-11-19. Scope: the paper-trading strategy as merged on `main`
(`ede8a34`). **No trading behaviour was changed by this task.** The only
additions are this report and a read-only analysis script
(`scripts/forensic_trades.py` + tests).

Evidence labels used throughout:

- **[CODE]** — read in the source at the path / function named.
- **[MEASURED]** — computed from on-chain data by the bot and reported by the
  operator from the VPS (`scripts/sniper_report.py` output, journal counts).
  The raw `decisions.jsonl` is on the VPS, not in this repository.
- **[ESTIMATE]** — a modelled value (paper cost model, assumptions).
- **[DERIVED]** — arithmetic on [MEASURED] figures, done for this report.
- **[UNKNOWN]** — not recorded anywhere; not reconstructed or guessed.

---

## 1. Architecture map (paper path, PumpSwap)

```
pool-sniper (bin/pool_sniper.rs, one process)
  spawn_gecko_poller ── GeckoTerminal new_pools, dex "pumpswap" ──► DiscoveryEvent::Pool
  read_listed_pool ──► amm::pumpswap::fetch_snapshot ──► require_canonical, vault/mint checks
  WatchList (pool_discovery.rs) ── watch 1800 s, recheck 30 s
  check_candidate ── mint authority / Token-2022 policy, candidate_sol_liquidity >= 50 SOL
  spawn_child ── ONE child at a time, max 20 children per UTC day
        │
solana-sniper --paper --amm pumpswap --single-trade --entry-window-secs 300 (main.rs)
  loop every SNIPER_LOOP_INTERVAL_MS (VPS: 5000), 1000 ms while a position is open
   ├─ resolve_paper_market_data → resolve_pumpswap_market_data
   │    fetch_snapshot (pool, vaults, mint, FeeConfig in one getMultipleAccounts)
   │    sqrt_price_x64 (reserve ratio as CLMM-style price), liquidity = real_quote
   │    paper_token_gates: mint/freeze authority, Token-2022 allow-list,
   │       holder gate (30 % / 70 % / sybil_holders) via onchain_risk
   ├─ price_series.push(signal::Observation)               signal.rs
   ├─ PaperSimulator::tick (main.rs)
   │    open position → exit_with_peak (strategy.rs) on spot ratio, liquidity_dropped,
   │                    max hold → book_close → sim_quote::paper_round_trip_cost
   │    no position  → SimpleSnipeStrategy::evaluate (strategy.rs): liquidity / mcap /
   │                    blocklist only → EntryPending
   ├─ EntryPending → signal_gate_rejects → signal::evaluate_at (score + vetoes)
   │               → max_spend cap → risk_manager.pre_trade_check (risk.rs)
   │               → confirm_entry at the observed spot sqrt price
   └─ DecisionRecord::save (decision.rs) → data/decisions/decisions.jsonl
analysis: scripts/sniper_report.py (aggregates), scripts/forensic_trades.py (per trade)
```

Live / dry-run share a different branch of `main.rs` (CLMM: exact quote via
`simulateTransaction`, `entry_economics`, value-based exits in
`manage_open_position`). PumpSwap has **no live path**; `--amm pumpswap` is
refused with `--live` (`validate_amm`). Dry-run on PumpSwap only simulates a
buy and a sell (`pumpswap_dry_run`).

## 2. Current strategy behaviour

| Stage | Rule | Where |
|---|---|---|
| Universe | canonical pump.fun PumpSwap pools, wSOL quote, not mayhem | `pumpswap::require_canonical`, `pool_fees` |
| Safety | mint/freeze revoked; Token-2022 metadata-only; top-1 ≤ 30 %, top-20 ≤ 70 %, < 5 identical top balances | `onchain_risk::classify_mint`, `holder_concentration_verdict` |
| Liquidity | withdrawable SOL ≥ 50 SOL (`SNIPER_MIN_LIQUIDITY_SOL`) | `check_candidate`, `SimpleSnipeStrategy::evaluate` |
| Entry signal | 34 samples warm-up; vetoes RSI ≥ 75, volatility ≥ 800 bps/sample, liquidity −10 % over 20 samples; score = 40·[EMA5>EMA20] + 30·[MACD hist>0] + 20·[45≤RSI<75] + 10·[Δliq≥0] ≥ 70 | `signal::evaluate` |
| Size | 0.01 SOL fixed | `StrategyConfig::default` |
| Exits | TP +10 %, SL −5 % on the **spot** price ratio; liquidity −50 %; max hold 3600 s; trailing stop off | `exit_with_peak`, `liquidity_dropped` |
| Costs (paper) | 2 × pool fee + c.p. impact (exit × 200 %) + 2 × 50 bps latency + 2 × network | `sim_quote::paper_round_trip_cost` |

Two properties matter for everything below:

1. **There is no expected-value check in paper.** `entry_economics`
   (`main.rs`) is only called on the live / dry-run branch. Paper enters on
   the signal alone. [CODE]
2. **The live "net edge" is not an expected value.** `entry_economics` passes
   `take_profit_bps` as the gross edge to `sim_quote::edge_breakdown`, i.e. it
   assumes every trade reaches the take-profit and never the stop. It is a
   cost sanity check, not an EV gate. [CODE]

## 3. Performance of all available samples

Source: operator-run `sniper_report.py` (2026-11-07, -09, -15) and journal
counts. Per-trade rows (exit action, net, MFE, MAE) were transcribed from
those outputs; their sums match the reported totals within rounding
(−0.003320 / −0.003319, −0.002190 / −0.002189, −0.003096 / −0.003101). [MEASURED]

| Sample (UTC) | Rules | Closed | Win | Gross | Est. cost | Net | PF | Stress −200 bps |
|---|---|---|---|---|---|---|---|---|
| S1 11-04 08:20 → 11-07 | holder 30/70 | 28 | 68 % | +0.006341 | 0.009660 | **−0.003319** | 0.79 | −0.008919 |
| S2 11-07 08:34 → 11-09 | + sybil gate | 13 | 54 % | +0.002174 | 0.004363 | **−0.002189** | 0.74 | −0.004789 |
| S3 11-09 06:47 → 11-15 | + 1 s exit polling | 38 | 53 % | +0.009967 | 0.013068 | **−0.003101** | 0.84 | −0.010701 |

Earlier CLMM samples (4 trades, October) are too old and on another venue;
they are not used.

Per-trade figures [DERIVED from MEASURED rows]:

| | S1 | S2 | S3 |
|---|---|---|---|
| gross move per trade (of 0.01 SOL) | +2.26 % | +1.67 % | +2.62 % |
| est. cost per trade | 3.45 % | 3.36 % | 3.44 % |
| avg win / avg loss (SOL) | +0.000658 / −0.001758 | +0.000902 / −0.001418 | +0.000804 / −0.001066 |
| break-even win rate vs observed | 72.8 % vs 67.9 % | 61.1 % vs 53.8 % | 57.0 % vs 52.6 % |
| stop overshoot beyond −5 % (median / max, bps) | 140 / 9496 | 211 / 1766 | 129 / 1604 |
| TP overshoot beyond +10 % (median / max, bps) | 22 / 199 | 100 / 1833 | 54 / 863 |
| losers that were ≥ +3 % first | 0 / 9 | 3 / 6 | 8 / 18 |
| losers that never went above +1.2 % | 8 / 9 | 3 / 6 | 9 / 18 |

Signal confidence buckets [MEASURED, n small]:

| Bucket | S1 (n, win, net) | S2 | S3 |
|---|---|---|---|
| 70–79 | 16, 81 %, +0.006011 | 3, 33 %, −0.001462 | 13, 54 %, +0.000201 |
| 80–89 | 1, 100 %, +0.000841 | — | 2, 50 %, −0.000496 |
| 90–99 | — | 3, 33 %, −0.002738 | 5, 20 %, −0.004265 |
| 100 | 11, 45 %, −0.010171 | 7, 71 %, +0.002010 | 18, 61 %, +0.001460 |

Funnel S3 (156 h) [MEASURED]: 2 418 pools watched → 2 291 "watch time
over" → 120 children → 38 entries. Child rejections: warming_up 2 106,
holder_concentration 1 590, sybil_holders 1 420, low_confidence 252,
veto_overbought 240, veto_volatility 179, veto_liquidity_draining 30.
RPC: 239 × HTTP 413, 301 × HTTP 429, 0 data errors.

## 4. Root causes of the negative result

Ranked by weight of evidence.

1. **Gross edge ≈ protocol fee.** [DERIVED] Across samples the gross spot move
   per trade is +1.7 % to +2.6 %. The PumpSwap fee on fresh pools is 1.25 % per
   side (first tier of the mainnet FeeConfig: 2 + 93 + 30 bps, decoded in
   `pumpswap::tests::decodes_the_live_mainnet_fee_config`) = 2.5 % per round
   trip [MEASURED from on-chain config]. The fee alone consumes the gross edge
   before any latency. Removing the 2 × 50 bps latency allowance entirely
   (an unrealistic best case) would change S1/S2/S3 to −0.000519 /
   −0.000889 / +0.000699 SOL and the three together to −0.000706 SOL
   [DERIVED]. The strategy has no measured edge that survives the fee.
2. **Payoff asymmetry from fixed exits.** [DERIVED] Wins are capped near
   +10 % gross (median TP overshoot 22–100 bps), losses average −10.7 % to −17.6 %
   net. Required win rate 57–73 %, observed 53–68 % in every sample.
3. **Tail losses not bounded by the stop.** [MEASURED] Stops fill past −5 % by a
   median 129–211 bps and up to −99.96 % (S1, the sybil rug, now gated) and
   −21 % (S3 with 1 s polling). The stop is evaluated on samples, not ticks;
   a real exit would add its own delay on top.
4. **Two loser populations, two different problems.** [DERIVED] In S3 half the
   losers (9/18) never rose above +1.2 % (entry problem), 8/18 were first
   ≥ +3 % and then hit the stop (exit problem: gains given back; the
   trailing stop exists but is off).
5. **The confidence score does not rank outcomes.** [MEASURED] The best and worst
   bucket swap between samples (S1: 100 worst; S3: 90–99 worst, 100 best). With
   four binary features the score takes only a few values (70, 80, 90, 100) and
   is computed on 5 s samples of a 3–5 minute window. No predictive value is
   shown; none is claimed.

What the data does **not** show (to keep it honest):

- **Costs are not over-estimated in the fee/impact part.** The dry-run on
  pool `5Fhn…oy3` matched the program to 0 bps for buy and sell
  (n = 1) [MEASURED]. The paper model's own fee term is a close approximation
  (see B-4 below).
- **The latency allowance (50 bps per leg) is unverified.** No real fill
  exists. Its order of magnitude is consistent with the median 1 s adverse
  move at stops in S3 (129 bps beyond the stop, which already includes one
  poll interval), so it is more likely under- than over-estimated on stop
  exits [DERIVED, weak].
- **Liquidity / volume vs outcome: not determined.** The per-pool liquidity
  path is in the raw records on the VPS (every post-snapshot record carries
  `liquidity` and `sqrt_price`); it was not available to this audit.
  `scripts/forensic_trades.py` computes it. **Volume is not recorded at
  all** [UNKNOWN].
- **Entry timing relative to pool creation: not determined.** Pool age at entry
  is not recorded [UNKNOWN] (see §5).

## 5. Missing data and instrumentation

| Gap | Consequence | Where |
|---|---|---|
| No indicator values on entry / reject records (only `signal_confidence` on entries, `reject_reason` + `samples` on rejects) | feature → outcome analysis impossible beyond the 4-feature score | `main.rs` paper `EntryPending` branch |
| No counterfactual (shadow) tracking of rejected or never-entered candidates | the value of each filter (holder, sybil, vetoes, score) cannot be measured; only survivors are seen | design |
| Discovery rejections (non-canonical, mayhem, Token-2022 extension, authority) only in journald, not in structured records | funnel analysis needs log scraping | `bin/pool_sniper.rs` |
| Pool creation time / listing time not carried to the child | entry timing vs pool age unknown | `pool_discovery::parse_gecko_pools` has `created_ms`; not forwarded |
| No trade volume, buy/sell counts, unique traders | order-flow hypotheses untestable | not collected |
| Fee tier per trade not recorded (only folded into `cost`) | cost breakdown by tier needs recomputation | exit record `cost` |
| MFE / MAE and stop fills only at poll resolution (5 s before PR #32, 1 s after) | extremes and overshoot are sample-based | `PaperSimulator::tick` |
| Time-to-MFE / time-to-MAE not recorded | entry vs exit timing analysis needs the raw path | `scripts/forensic_trades.py` reconstructs it from hold records |
| No real fills (no live, no PumpSwap live path) | latency and MEV cost remain assumptions | by design |

## 6. Confirmed bugs vs design limitations

Confirmed defects (code reading, reproducible):

- **B-1 Discovery sampling bias.** `bin/pool_sniper.rs` main loop: candidate
  checks run only when no child is running (`if running.is_none()`) **and**
  the daily cap is not reached (`disc_state.daily.reached(now,
  max_pools_per_day)`, default 20). Pools that qualify while a child is busy,
  or after the 20th pool of the UTC day, expire unevaluated. S3: 2 291 of
  2 418 watched pools expired; 120 children in 156 h is 18.4 per day on
  average against a cap of 20 [DERIVED from MEASURED counts], so the cap
  is reached on busy days (per-day counts were not collected). The traded sample is "first pools that
  qualify while idle, early in the UTC day", not the opportunity set; and
  "watch time over" does not mean "never reached 50 SOL". [CODE + MEASURED]
- **B-2 Paper/live decision parity.** Paper exits on the spot price ratio
  (`PaperSimulator::tick` → `exit_with_peak(ratio)`), live exits on the value
  of selling the held tokens against the SOL swapped in
  (`manage_open_position` → `value_ratio_scaled(swapped_in,
  quote.expected_output)`; the adapter quote includes the pool fee). That
  ratio contains **both** legs' fees: with 1.25 % per side, the live
  take-profit (+10 % value) needs roughly +12.8 % in price and the live
  stop-loss (−5 % value) fires at roughly −2.6 % in price (a fresh live position already shows −2.5 % value from fees), where paper uses
  +10 % / −5 % in price. Paper outcomes are therefore not what the same rules
  would produce live (more stops, fewer take-profits live). Paper also has no
  `entry_economics` gate. [CODE; the percentages are DERIVED]
- **B-3 Live "net edge" assumes the take-profit.** `entry_economics` →
  `edge_breakdown { take_profit_bps, .. }`: the expected gross is the TP, not a
  probability-weighted outcome. [CODE]
- **B-4 Paper fee on notional.** `paper_round_trip_cost` charges
  `2 × position × fee` regardless of the exit value; the exit fee is really
  on the exit amount. Error ≈ fee × move: +0.125 % of the position on a +10 %
  win, −0.06 % on a −5 % loss. Small; it slightly flatters winners. [CODE]
- **B-5 Report window boundary.** `sniper_report.py --hours` filters by record
  time, so an exit inside the window whose entry is before it is counted
  (a sample can contain one trade opened under the previous rules). [CODE]
- **B-6 Report label.** The per-pool `pnl=` lines are gross
  (`realized_simulated_pnl_lamports`); the summary below them is net. [CODE]

Design limitations (behave as designed, limit the strategy):

- **L-1** No expected-value model; fixed 0.01 SOL, +10 % / −5 %.
- **L-2** Signal: 4 binary features on a 34-sample (≈ 170 s at 5 s) warm-up
  inside a 300 s entry window; entries fall 170–300 s after the child starts.
- **L-3** Stops at poll resolution; no tick data.
- **L-4** Paper position not persisted across restarts (documented).
- **L-5** Paper fills at the observed spot; competition, MEV and failed
  transactions only through the flat latency allowance and the stress line.
- **L-6** Holder data limited to `getTokenLargestAccounts` top-20; sybil gate is
  exact-equality only (a split with varied amounts evades it).
- **L-7** PumpSwap has no live path; live-readiness evidence (latency, fills)
  cannot exist for it.

Checked and **not** found:

- Price orientation: `held_token_price(_, input_is_token0 = false)` and
  `sqrt_price_x64` give SOL per token for PumpSwap; TP fires on a rising price
  (tests `sqrt_price_follows_the_reserve_ratio`, report MFE signs consistent).
- Look-ahead: the signal uses samples up to the decision; the entry price is
  the last sample; exits use later samples only. No future data is read.
- MFE/MAE arithmetic: peak/trough of `price_ratio_scaled` updated before the
  exit decision on each tick (`PaperSimulator::tick`); correct at sample
  resolution.
- Duplicate cost counting: none in paper (fee, impact, latency, network
  each once per leg); the dry-run double-checks fee + impact against the
  program.
- Incomplete trades: the S1–S3 reports showed no entries without an exit and
  no duplicate exits.

## 7. Prioritised issues

| ID | Priority | Issue | Type |
|---|---|---|---|
| P0-1 | P0 | No measured edge after the 2.5 % PumpSwap round-trip fee; fixed TP/SL payoff needs 57–73 % wins | strategy |
| P0-2 | P0 | Discovery sampling bias (B-1): the sample is not the opportunity set | bug |
| P0-3 | P0 | No feature logging and no shadow outcomes for rejected / not-entered candidates: no hypothesis can be validated | instrumentation |
| P1-1 | P1 | Paper/live parity: spot vs value exits, no paper EV gate (B-2) | bug |
| P1-2 | P1 | Live edge gate assumes TP (B-3) | bug |
| P1-3 | P1 | Loser split: 50 % straight-down entries, 44 % gave back ≥ +3 % (S3) | strategy |
| P1-4 | P1 | Stop tail beyond −5 % (median +129–211 bps, max −21 % with 1 s polling) | strategy / data |
| P1-5 | P1 | Pool age, volume, flow not recorded | instrumentation |
| P2-1 | P2 | Fee on notional in paper cost model (B-4) | bug (small) |
| P2-2 | P2 | Report window boundary and gross label (B-5, B-6) | bug (reporting) |
| P2-3 | P2 | Paper position persistence (L-4) | limitation |
| P2-4 | P2 | 413 / 429 noise (no data errors so far) | ops |

## 8. Recommended Alpha Engine architecture (not implemented)

Rust, inside `solana-sniper`, reusing the existing data, risk and HSM layers
unchanged; paper and live call the same functions.

1. **Candidate ledger** (`alpha/ledger.rs`): one record per pool from the first
   on-chain read, with every gate verdict (pass / reason) and the full feature
   vector at each decision point; shadow outcome = price path for N minutes
   after the decision whether or not it was entered.
2. **Feature extraction** (`alpha/features.rs`): pure functions over the
   snapshot series — pool age (GeckoTerminal `created_ms` + first on-chain read),
   liquidity level and slope, reserve-ratio returns, realised volatility,
   holder distribution metrics, fee tier, market cap. Volume / flow only if a
   measured source is added (none today).
3. **Cost model** (`alpha/costs.rs`): fee tier from `pumpswap::pool_fees` on the
   actual entry and exit amounts (fixes B-4), impact from
   `pumpswap::buy_exact_quote_in` / `sell_exact_base_in` (exact, already
   verified against the program), latency and failure terms as explicit,
   versioned assumptions until measured.
4. **EV gate** (`alpha/ev.rs`): enter only if a pre-registered estimate of
   `p(TP) × win − p(SL) × loss − costs` is positive with margin, where the
   probabilities come from a held-out, earlier time window of the ledger — no
   in-sample fitting, no ML until a baseline beats costs out of sample.
5. **Exit policy** (`alpha/exits.rs`): same function for paper and live,
   value-based (sell quote incl. fee) in both (fixes B-2), trailing / time
   exits as named, versioned variants.
6. **Discovery** without the B-1 bias: evaluate candidates independently of a
   running child (record and shadow all; the single-position limit applies to
   entries, not to observation).
7. **Replay harness** (`alpha/replay.rs`): deterministic re-run of any variant
   over recorded ledger snapshots, so a variant is compared on the same
   candidates before it ever trades paper.

## 9. Testing methodology

- **Pre-registration:** every variant is written down (rule, parameters, success
  and stop criteria) before data collection; one variant per experiment.
- **Time-split data:** build on window A, select on window B, judge once on a
  later window C. No parameter is tuned on C.
- **Replay first, paper second:** a variant must be positive in replay on the
  ledger before a paper sample is spent on it.
- **Statistics:** per-trade net in SOL; 95 % bootstrap CI clustered by UTC day;
  stress case (exits 200 bps worse) reported with every result; minimum 100
  closed trades per arm before a verdict; compare against a fixed baseline
  (current rules) on the same candidates.
- **Parity tests:** unit tests that paper and live call the same entry, exit and
  cost functions for the same snapshot.
- **No-look-ahead tests:** replay asserts that a decision at time t reads no
  sample with timestamp > t.
- **Cost validation:** periodic dry-runs on the traded pools; fee + impact must
  stay within 5 bps of the program; latency stays labelled as an assumption
  until real fills exist.

## 10. Conditions for proceeding to implementation

Implementation of a new strategy should start only when all hold:

1. This report is reviewed and the P0 list is accepted (or amended) by the
   operator.
2. Instrumentation first: P0-2 (discovery bias) and P0-3 (candidate ledger with
   features and shadow outcomes) are implemented and collect at least one
   week of data **without any trading-rule change**.
3. A written hypothesis exists that addresses the fee structure (P0-1) with a
   mechanism, e.g. only higher-market-cap fee tiers, or a payoff that is
   larger relative to 2.5 % round-trip — not a re-tuned TP/SL on S1–S3.
4. Success / stop rule fixed in advance, e.g. "on window C, ≥ 100 trades, net
   > 0 with the clustered 95 % CI lower bound > 0 and the stress case ≥ 0;
   otherwise stop".
5. Live remains disabled; nothing here changes `RiskConfig` limits, the HSM
   path, or any fail-closed check.

## Appendix: how to reproduce on the VPS

```bash
python3 /root/solana-sniper-soldier/scripts/forensic_trades.py /var/lib/solana-hft --since 2026-11-09T06:47:31Z
python3 /root/solana-sniper-soldier/scripts/forensic_trades.py /var/lib/solana-hft --since 2026-11-04T08:20:00Z --rows
```

Output: winners vs losers on liquidity at entry, liquidity change in the 60 s
before entry and during the hold, confidence, entry hour, hold time,
MFE / MAE, time to ±3 %, stop overshoot. It is descriptive (medians, n); it
makes no significance claim.
