//! # Solana HFT Platform — Production Binary
//!
//! ## Modes
//! - **Simulation mode** (default): Runs the HFT loop with mock data, records decisions.
//! - **Dry-run mode** (`--dry-run`): Builds and signs real transactions but never sends them.
//! - **Live mode** (`--live`): Reads wallet.json, connects to real RPC, executes real trades.
//!
//! ## Safety
//! - `--dry-run` flag ensures no real transactions are submitted.
//! - RiskManager guards (max trade size, slippage, daily loss) run before every trade.
//! - BlockhashManager + send_with_retry provide 3-level exponential backoff.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::time::sleep;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

// Library modules (solana_sniper crate): compiled once, shared with the
// other binaries (pool-sniper, tools).
use amm::AmmAdapter;
use hw_signer::SignerAdapter;
use remote_hsm::RemoteHsmSigner;
use solana_sniper::{
    amm, decision, discovery, hw_signer, jito, marketdata, metrics, onchain_risk, position, redact,
    remote_hsm, retry, risk, signal, sim_quote, strategy,
};

// ============================================================================
// AŞAMA 2/3/4 — Paper trading on REAL on-chain market data (no tx ever built)
// ============================================================================

/// Simulated paper position: the real entry sqrt price and the position size
/// that would have been traded. Max ONE open simulated position — mirrors the
/// RiskManager single-position rule WITHOUT writing to the risk state file
/// (a paper position must never leak into live restart state).
#[derive(Clone, Copy)]
struct PaperPosition {
    entry_sqrt_price: u128,
    position_size_lamports: u64,
    /// SOL-side liquidity at entry (liquidity-drop exit, as in live).
    entry_liquidity_lamports: u64,
    opened_at_ms: u64,
    /// Best / worst price ratio (scaled, see `strategy::RATIO_SCALE`) seen
    /// since entry: the trailing stop and the MFE/MAE of the record.
    peak_ratio: u128,
    trough_ratio: u128,
}

/// Outcome of one paper iteration against real market data.
#[derive(Debug)]
enum PaperTick {
    /// Simulated position closed at the simulated stop-loss (realized pnl).
    ClosedStopLoss(i64),
    /// Simulated position closed at the simulated take-profit (realized pnl).
    ClosedTakeProfit(i64),
    /// Simulated position closed by the max holding time, a SOL-side
    /// liquidity drop or the opt-in trailing stop — the same exits live
    /// applies.
    ClosedForced(strategy::ExitDecision, i64),
    /// An open simulated position is held (no exit signal).
    Hold,
    /// No open position and the strategy produced no entry signal.
    NoEntrySignal,
    /// No open position and the strategy produced an entry signal; the caller
    /// must run the risk gates (max-spend + pre_trade_check) before calling
    /// `confirm_entry`.
    EntryPending(strategy::EntrySignal),
}

/// Pure paper-trading state machine (no I/O, no randomness): real signals
/// only. `entries` increments ONLY on a confirmed entry (AŞAMA 3) — never
/// unconditionally per iteration.
struct PaperSimulator {
    strategy: strategy::SimpleSnipeStrategy,
    position: Option<PaperPosition>,
    entries: u64,
    exits_stop_loss: u64,
    exits_take_profit: u64,
    holds: u64,
    no_entry_signals: u64,
    simulated_pnl_lamports: i64,
    /// Swap direction of the configured pool: true when the pay-side
    /// (input) mint is the pool's token0. The pool price is token1/token0,
    /// so with token0 as input a RISING price means the held token is
    /// worth LESS — exits and P&L must use the inverted ratio.
    input_is_token0: bool,
    /// Wall clock for this tick (ms) — set by the caller; 0 in tests.
    now_ms: u64,
    /// Max holding time (ms) of a simulated position; 0 = no limit.
    max_hold_ms: u64,
    exits_forced: u64,
    /// Liquidity seen by the last tick (recorded on confirm_entry).
    last_liquidity_lamports: u64,
    /// Cost estimate applied to every close (fees, impact, latency, network).
    cost_model: sim_quote::PaperCostModel,
    /// Sum of the gross P&L minus the estimated costs.
    net_pnl_lamports: i64,
    /// Sum of the estimated costs.
    costs_lamports: u64,
    /// The last close: (gross, net, cost breakdown, hold time ms).
    last_close: Option<PaperClose>,
}

/// One closed paper position.
#[derive(Debug, Clone, Copy)]
struct PaperClose {
    gross_pnl_lamports: i64,
    net_pnl_lamports: i64,
    cost: sim_quote::PaperCost,
    hold_ms: u64,
    /// Best / worst move since entry (bps): did a loser go into profit
    /// first (exit problem) or fall straight away (entry problem)?
    mfe_bps: i64,
    mae_bps: i64,
}

impl PaperSimulator {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_config(strategy::StrategyConfig::default())
    }

    fn with_config(config: strategy::StrategyConfig) -> Self {
        Self {
            strategy: strategy::SimpleSnipeStrategy::new(config),
            position: None,
            entries: 0,
            exits_stop_loss: 0,
            exits_take_profit: 0,
            holds: 0,
            no_entry_signals: 0,
            simulated_pnl_lamports: 0,
            input_is_token0: false,
            now_ms: 0,
            max_hold_ms: 0,
            exits_forced: 0,
            last_liquidity_lamports: 0,
            cost_model: sim_quote::PaperCostModel {
                fee_rate_ppm: 0,
                exit_impact_pct: 100,
                latency_bps_per_leg: 0,
                network_cost_per_tx_lamports: 0,
            },
            net_pnl_lamports: 0,
            costs_lamports: 0,
            last_close: None,
        }
    }

    /// Book a close: gross from the spot move, net after the cost estimate.
    fn book_close(&mut self, pos: &PaperPosition, gross: i64, exit_liquidity: u64) {
        let cost = sim_quote::paper_round_trip_cost(
            &self.cost_model,
            pos.position_size_lamports,
            pos.entry_liquidity_lamports,
            exit_liquidity,
        );
        let net = gross.saturating_sub(i64::try_from(cost.total_lamports).unwrap_or(i64::MAX));
        self.simulated_pnl_lamports = self.simulated_pnl_lamports.saturating_add(gross);
        self.net_pnl_lamports = self.net_pnl_lamports.saturating_add(net);
        self.costs_lamports = self.costs_lamports.saturating_add(cost.total_lamports);
        self.last_close = Some(PaperClose {
            gross_pnl_lamports: gross,
            net_pnl_lamports: net,
            cost,
            hold_ms: self.now_ms.saturating_sub(pos.opened_at_ms),
            mfe_bps: strategy::ratio_to_bps(pos.peak_ratio),
            mae_bps: strategy::ratio_to_bps(pos.trough_ratio),
        });
    }

    fn has_open_position(&self) -> bool {
        self.position.is_some()
    }

    /// Open position audit fields (entry sqrt price + size) — read BEFORE
    /// `tick` so an exit can be persisted with the entry it was computed
    /// from (tick clears the position on close).
    fn open_position(&self) -> Option<(u128, u64)> {
        self.position
            .map(|p| (p.entry_sqrt_price, p.position_size_lamports))
    }

    /// One iteration tick against a REAL current sqrt price. Exit decisions
    /// (SimpleSnipeStrategy::should_exit — unchanged) run first while a
    /// simulated position is open; otherwise the unchanged entry strategy
    /// (`evaluate`) runs with the real candidate.
    fn tick(
        &mut self,
        candidate: &strategy::TokenCandidate,
        current_sqrt_price: u128,
    ) -> PaperTick {
        self.last_liquidity_lamports = candidate.liquidity_lamports;
        if let Some(mut pos) = self.position {
            // Orient the ratio so "up" always means the held token gained
            // value in input units: (current/entry)^2 for token1 input,
            // (entry/current)^2 for token0 input (args swapped).
            let (from_sqrt, to_sqrt) = if self.input_is_token0 {
                (current_sqrt_price, pos.entry_sqrt_price)
            } else {
                (pos.entry_sqrt_price, current_sqrt_price)
            };
            // Same integer ratio and exit rule as live (exit_with_peak); an
            // unrepresentable ratio decides nothing (Hold).
            let mut exit = match strategy::price_ratio_scaled(from_sqrt, to_sqrt) {
                Some(ratio) => {
                    pos.peak_ratio = pos.peak_ratio.max(ratio);
                    pos.trough_ratio = pos.trough_ratio.min(ratio);
                    self.position = Some(pos);
                    self.strategy.exit_with_peak(ratio, pos.peak_ratio)
                }
                None => strategy::ExitDecision::Hold,
            };
            if exit == strategy::ExitDecision::Hold {
                if strategy::liquidity_dropped(
                    pos.entry_liquidity_lamports,
                    candidate.liquidity_lamports,
                ) {
                    exit = strategy::ExitDecision::LiquidityDrop;
                } else if self.max_hold_ms > 0
                    && self.now_ms.saturating_sub(pos.opened_at_ms) >= self.max_hold_ms
                {
                    exit = strategy::ExitDecision::MaxHoldTime;
                }
            }
            return match exit {
                strategy::ExitDecision::Hold => {
                    self.holds += 1;
                    PaperTick::Hold
                }
                strategy::ExitDecision::MaxHoldTime
                | strategy::ExitDecision::LiquidityDrop
                | strategy::ExitDecision::TrailingStop => {
                    let pnl =
                        simulated_pnl_lamports(from_sqrt, to_sqrt, pos.position_size_lamports);
                    self.book_close(&pos, pnl, candidate.liquidity_lamports);
                    self.exits_forced += 1;
                    self.position = None;
                    PaperTick::ClosedForced(exit, pnl)
                }
                strategy::ExitDecision::StopLoss => {
                    let pnl =
                        simulated_pnl_lamports(from_sqrt, to_sqrt, pos.position_size_lamports);
                    self.book_close(&pos, pnl, candidate.liquidity_lamports);
                    self.exits_stop_loss += 1;
                    self.position = None;
                    PaperTick::ClosedStopLoss(pnl)
                }
                strategy::ExitDecision::TakeProfit => {
                    let pnl =
                        simulated_pnl_lamports(from_sqrt, to_sqrt, pos.position_size_lamports);
                    self.book_close(&pos, pnl, candidate.liquidity_lamports);
                    self.exits_take_profit += 1;
                    self.position = None;
                    PaperTick::ClosedTakeProfit(pnl)
                }
            };
        }
        match self.strategy.evaluate(candidate, current_sqrt_price) {
            Some(sig) => PaperTick::EntryPending(sig),
            None => {
                self.no_entry_signals += 1;
                PaperTick::NoEntrySignal
            }
        }
    }

    /// Confirm a pending entry AFTER the caller's risk gates passed. The only
    /// place `entries` increments (AŞAMA 3: total_trades semantics).
    fn confirm_entry(&mut self, entry_sqrt_price: u128, sig: strategy::EntrySignal) {
        self.position = Some(PaperPosition {
            entry_sqrt_price,
            position_size_lamports: sig.position_size_lamports,
            entry_liquidity_lamports: self.last_liquidity_lamports,
            opened_at_ms: self.now_ms,
            peak_ratio: strategy::RATIO_SCALE,
            trough_ratio: strategy::RATIO_SCALE,
        });
        self.entries += 1;
    }
}

/// Quote-based simulated P&L for a closed paper position (no real swap): the
/// price ratio `(current/entry)^2` from `strategy::price_ratio_scaled` — the
/// SAME integer value `SimpleSnipeStrategy::should_exit` compares against
/// the stop-loss / take-profit thresholds — applied to the position size by
/// `strategy::pnl_from_ratio` (overflow-checked, losses rounded away from
/// zero). A zero entry or an unrepresentable ratio yields 0: `should_exit`
/// never closes on such data either. Always reported/labeled as simulated,
/// never mixed with realized on-chain P&L.
fn simulated_pnl_lamports(
    entry_sqrt_price: u128,
    current_sqrt_price: u128,
    position_size_lamports: u64,
) -> i64 {
    strategy::price_ratio_scaled(entry_sqrt_price, current_sqrt_price).map_or(0, |ratio| {
        strategy::pnl_from_ratio(ratio, position_size_lamports)
    })
}

/// Real market snapshot for one paper iteration — the SAME sources and
/// functions the live path uses: WS feed or `PoolPriceFeed` (pool-state sqrt
/// price + snapshot age), the staleness gate, swap-account resolution (real
/// vault addresses), mint/freeze authority checks and, with `--live-risk-data`,
/// real vault liquidity + holder concentration.
struct PaperMarketSnapshot {
    current_sqrt_price: u128,
    liquidity_lamports: u64,
    market_cap_lamports: u64,
    holders: u64,
    is_blocklisted: bool,
    source: &'static str,
    /// True when the input (pay-side) mint is the pool's token0.
    input_is_token0: bool,
    /// The pool's trade fee (ppm) from its AmmConfig.
    trade_fee_ppm: u64,
}

/// Paper market-data failure. Data that could not be fetched at all is
/// `Unavailable` (paper mode has no circuit breaker and never mutates risk
/// state — it is counted separately, never as "simulated success"). Data that
/// WAS fetched but violates a real risk rule is a genuine `RejectReason`
/// (stale_price, token_authority_risk).
enum PaperDataError {
    Unavailable(String),
    Rejected(risk::RejectReason),
}

/// AŞAMA 2 — fetch real market data for a paper iteration. Fail-closed: a
/// fetch error or a stale price never produces a "simulated success"; the
/// iteration is rejected with the real reason. Paper mode never builds,
/// signs or sends a transaction.
fn resolve_paper_market_data(
    args: &Args,
    risk_cfg: &risk::RiskConfig,
    rpc_client: &Arc<RpcClient>,
    ws_provider: Option<&hft_marketdata::solana_ws::SolanaWsProvider>,
    blocklist: &std::collections::HashSet<Pubkey>,
) -> Result<PaperMarketSnapshot, PaperDataError> {
    let pool_id_str = args
        .pool_id
        .as_deref()
        .expect("--paper requires --pool-id (validated at startup)");
    let pool_id = Pubkey::from_str(pool_id_str)
        .map_err(|e| PaperDataError::Unavailable(format!("invalid --pool-id: {e}")))?;
    let input_mint_str = args
        .input_mint
        .as_deref()
        .expect("--paper requires --input-mint (validated at startup)");
    let output_mint_str = args
        .output_mint
        .as_deref()
        .expect("--paper requires --output-mint (validated at startup)");
    let input_mint = Pubkey::from_str(input_mint_str)
        .map_err(|e| PaperDataError::Unavailable(format!("invalid --input-mint: {e}")))?;
    let output_mint = Pubkey::from_str(output_mint_str)
        .map_err(|e| PaperDataError::Unavailable(format!("invalid --output-mint: {e}")))?;

    // Same cluster switch as the live path: devnet CLMM program id when the
    // RPC is devnet, the mainnet program id otherwise.
    let program_id = if args.rpc.contains("devnet") {
        Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID_DEVNET)
            .expect("valid devnet program id")
    } else {
        Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID)
            .expect("valid mainnet program id")
    };

    // 1) Real price: prefer a fresh WS update, else the fail-closed
    //    RPC-polled pool state feed (identical to the live path).
    let ws_state = ws_provider
        .filter(|p| p.is_connected())
        .and_then(|p| p.get_pool_state(pool_id_str));
    let (current_sqrt_price, price_timestamp_ms, source) = if let Some(state) = ws_state {
        (state.sqrt_price, state.timestamp_ms, "websocket")
    } else {
        let feed = marketdata::PoolPriceFeed::new(rpc_client.clone(), pool_id, program_id);
        let pool = feed.refresh().map_err(|e| {
            PaperDataError::Unavailable(format!(
                "pool state fetch failed (no synthetic price is ever used): {e}"
            ))
        })?;
        let ts = feed
            .age_ms()
            .map(|age| marketdata::now_ms().saturating_sub(age))
            .unwrap_or(0);
        (pool.sqrt_price_x64, ts, "rpc_poll")
    };

    // 2) Staleness gate — the SAME function the live path calls.
    if let Err(reason) =
        risk::check_price_staleness(price_timestamp_ms, risk_cfg.price_staleness_ms)
    {
        return Err(PaperDataError::Rejected(reason));
    }

    // 3) Swap-account resolution (real vault addresses). Paper never builds a
    //    transaction, so the user ATA's are unused — the dummy owner only
    //    derives them deterministically (the dry-run path uses the same
    //    all-zero-owner pattern).
    //    Paper uses only the pool's identity from it (vaults, orientation,
    //    fee tier), which does not change, so it is cached briefly instead
    //    of costing 3-4 RPC calls every iteration (see PaperAccountsCache).
    let dummy_user = Pubkey::new_from_array([0u8; 32]);
    let key = (pool_id, input_mint, output_mint);
    let now = marketdata::now_ms() as u64;
    let cached = paper_accounts_cache().lock().unwrap().get(&key, now);
    let (accounts, pool) = match cached {
        Some(v) => v,
        None => {
            let v = amm::account_resolver::resolve_swap_accounts(
                rpc_client,
                &pool_id,
                &dummy_user,
                &input_mint,
                &output_mint,
                &program_id,
            )
            .map_err(|e| {
                PaperDataError::Unavailable(format!("swap account resolution failed: {e}"))
            })?;
            paper_accounts_cache()
                .lock()
                .unwrap()
                .put(key, now, v.clone());
            v
        }
    };

    // 4) Mint/freeze authority rug-check — same as live: a present mint or
    //    freeze authority rejects the trade (fail-closed).
    for mint in [input_mint, output_mint] {
        match onchain_risk::fetch_mint_authority_risk_cached(rpc_client, &mint) {
            Ok(risk_info) if risk_info.is_risky() => {
                return Err(PaperDataError::Rejected(
                    risk::RejectReason::TokenAuthorityRisk,
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(PaperDataError::Unavailable(format!(
                    "mint authority check failed for {mint}: {e}"
                )));
            }
        }
    }

    // 5) Real vault liquidity + holder concentration with --live-risk-data
    //    (same functions as live); otherwise the explicit CLI values.
    let mut liquidity_lamports = args.pool_liquidity;
    let mut holders = args.pool_holders;
    let mut top_holder_pct: f64 = 0.0;
    if args.live_risk_data {
        liquidity_lamports = onchain_risk::fetch_vault_liquidity(rpc_client, &accounts.input_vault)
            .map_err(|e| {
                PaperDataError::Unavailable(format!("vault liquidity fetch failed: {e}"))
            })?;
        // Exclude the pool's own vaults (infrastructure accounts resolved by
        // resolve_swap_accounts) so the pool never counts as a "holder".
        // Holder risk is measured on the token being BOUGHT (output mint),
        // never on the pay-side mint (wSOL has no meaningful holder set).
        let vaults = [accounts.input_vault, accounts.output_vault];
        let stats = onchain_risk::fetch_holder_stats_cached(rpc_client, &output_mint, &vaults)
            .map_err(|e| PaperDataError::Unavailable(format!("holder stats fetch failed: {e}")))?;
        // Holder-concentration gate (operator-approved design, fail-closed):
        // single holder >30% or combined top-20 >70% of supply rejects with
        // an explicit reason — never a silent no_entry_signal. Unassessable
        // data (no measurable non-excluded holders) rejects the same way.
        match onchain_risk::holder_concentration_verdict(&stats) {
            onchain_risk::HolderVerdict::Ok => {}
            onchain_risk::HolderVerdict::SingleHolderConcentrated(pct) => {
                tracing::warn!(
                    target: "paper",
                    mint = %output_mint,
                    top_holder_pct = pct,
                    program_owned_pct = stats.program_owned_pct,
                    threshold_pct = onchain_risk::MAX_SINGLE_HOLDER_PCT,
                    "holder concentration gate: single holder exceeds threshold"
                );
                return Err(PaperDataError::Rejected(
                    risk::RejectReason::HolderConcentrationExceeded,
                ));
            }
            onchain_risk::HolderVerdict::TopHoldersConcentrated(pct) => {
                tracing::warn!(
                    target: "paper",
                    mint = %output_mint,
                    top20_holder_pct = pct,
                    program_owned_pct = stats.program_owned_pct,
                    threshold_pct = onchain_risk::MAX_TOP20_HOLDER_PCT,
                    "holder concentration gate: top-20 holders exceed threshold"
                );
                return Err(PaperDataError::Rejected(
                    risk::RejectReason::HolderConcentrationExceeded,
                ));
            }
            onchain_risk::HolderVerdict::Unassessable => {
                tracing::warn!(
                    target: "paper",
                    mint = %output_mint,
                    "holder concentration gate: no measurable holders (fail-closed reject)"
                );
                return Err(PaperDataError::Rejected(
                    risk::RejectReason::HolderConcentrationExceeded,
                ));
            }
        }
        holders = stats.sampled_holders;
        top_holder_pct = stats.top_holder_pct;
    }
    let live_blocklisted = blocklist.contains(&input_mint) || blocklist.contains(&output_mint);
    let is_blocklisted = args.pool_blocklisted
        || live_blocklisted
        || (args.live_risk_data && top_holder_pct > args.max_top_holder_pct);

    Ok(PaperMarketSnapshot {
        current_sqrt_price,
        liquidity_lamports,
        market_cap_lamports: args.pool_market_cap,
        holders,
        is_blocklisted,
        source,
        input_is_token0: accounts.input_vault == pool.token_vault_0,
        // Unknown fee = the highest CLMM tier (never underestimate costs).
        trade_fee_ppm: pool.trade_fee_rate.map(u64::from).unwrap_or(10_000),
    })
}

/// How long paper reuses a pool's resolved accounts. Only identity data is
/// used from them (vault addresses, mints, AmmConfig fee tier); the price
/// and the vault balance are read fresh every iteration.
const PAPER_ACCOUNTS_TTL_MS: u64 = 300_000;

type PaperAccountsKey = (Pubkey, Pubkey, Pubkey);
type PaperAccounts = (
    amm::raydium_v4::SwapAccounts,
    amm::account_resolver::ResolvedPool,
);

/// Single-entry time-limited cache (one paper process trades one pool).
/// Failures are never stored; a different key or an expired entry misses.
struct TtlSlot<K, V> {
    slot: Option<(K, u64, V)>,
    ttl_ms: u64,
}

impl<K: PartialEq, V: Clone> TtlSlot<K, V> {
    const fn new(ttl_ms: u64) -> Self {
        Self { slot: None, ttl_ms }
    }

    fn get(&self, key: &K, now_ms: u64) -> Option<V> {
        match &self.slot {
            Some((k, at, v)) if k == key && now_ms >= *at && now_ms - at < self.ttl_ms => {
                Some(v.clone())
            }
            _ => None,
        }
    }

    fn put(&mut self, key: K, now_ms: u64, value: V) {
        self.slot = Some((key, now_ms, value));
    }
}

fn paper_accounts_cache() -> &'static std::sync::Mutex<TtlSlot<PaperAccountsKey, PaperAccounts>> {
    static CACHE: std::sync::Mutex<TtlSlot<PaperAccountsKey, PaperAccounts>> =
        std::sync::Mutex::new(TtlSlot::new(PAPER_ACCOUNTS_TTL_MS));
    &CACHE
}

/// HFT Platform CLI arguments.
#[derive(Parser, Debug)]
#[command(
    name = "solana-sniper",
    about = "Solana HFT Platform — ultra-low-latency trading"
)]
struct Args {
    /// RPC endpoint URL
    #[arg(
        long,
        env = "SNIPER_RPC",
        default_value = "https://api.devnet.solana.com"
    )]
    rpc: String,

    /// WebSocket endpoint URL
    #[arg(long, env = "SNIPER_WS", default_value = "wss://api.devnet.solana.com")]
    ws: String,

    /// Path to wallet.json
    #[arg(long, env = "SNIPER_WALLET", default_value = "./wallet.json")]
    wallet: PathBuf,

    /// Dry-run: build + sign transactions, print them, never send
    #[arg(long, env = "SNIPER_DRY_RUN", default_value_t = false)]
    dry_run: bool,

    /// Live mode: actually submit transactions to the network
    #[arg(long, env = "SNIPER_LIVE", default_value_t = false)]
    live: bool,

    /// Paper-trading mode: run the strategy on simulated market data, never
    /// submit any on-chain transaction. Mutually exclusive with live/dry-run
    /// (enforced by `validate_signing_config`, which also covers values that
    /// come from SNIPER_* env vars, e.g. SNIPER_LIVE=false).
    #[arg(long, env = "SNIPER_PAPER", default_value_t = false)]
    paper: bool,

    /// Number of loop iterations. 0 = run until stopped (SIGTERM / Ctrl-C),
    /// which is what the systemd service uses.
    #[arg(long, env = "SNIPER_ITERATIONS", default_value_t = 30)]
    iterations: u32,

    /// Pause between loop iterations in milliseconds (minimum 50). Raise it
    /// on rate-limited RPC plans: every iteration makes several RPC calls.
    #[arg(long, env = "SNIPER_LOOP_INTERVAL_MS", default_value_t = 200)]
    loop_interval_ms: u64,

    /// Optional blockhash (base58) to use for dry-run. When set, dry-run does
    /// not contact the RPC for a blockhash (deterministic / offline smoke tests).
    #[arg(long)]
    blockhash: Option<String>,

    /// Data directory for decision records, logs, and metrics
    #[arg(long, env = "SNIPER_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Remote HSM endpoint (e.g. https://127.0.0.1:8443). When set, transactions are signed via the remote HSM using mTLS.
    #[arg(long, env = "SNIPER_HSM_ENDPOINT")]
    hsm_endpoint: Option<String>,

    /// CA certificate (PEM) used to verify the HSM server (mTLS)
    #[arg(long, env = "SNIPER_HSM_CA")]
    hsm_ca: Option<PathBuf>,

    /// Combined client cert + key PEM presented to the HSM server (mTLS)
    #[arg(long, env = "SNIPER_HSM_CLIENT_IDENTITY")]
    hsm_client_identity: Option<PathBuf>,

    /// Pinned wallet public key (base58) the HSM must sign with. Required in
    /// LIVE mode: if the HSM reports any other key (e.g. it fell back to an
    /// ephemeral dev key) the bot refuses to trade (fail-closed).
    #[arg(long, env = "SNIPER_EXPECTED_SIGNER")]
    expected_signer: Option<String>,

    /// Maximum time (seconds) a live position is held. When reached the
    /// position is sold regardless of price. 0 disables the limit.
    #[arg(long, env = "SNIPER_MAX_HOLD_SECS", default_value_t = 86_400)]
    max_hold_secs: u64,

    /// DRY-RUN only: build the swap with this funded address as fee payer and
    /// simulate it on-chain WITHOUT signing (sigVerify off). Validates the
    /// complete swap on mainnet before your own wallet is funded. Nothing is
    /// signed or sent.
    #[arg(long, env = "SNIPER_SIMULATE_PAYER")]
    simulate_payer: Option<String>,

    /// Address for the Prometheus `/metrics` + `/health` HTTP server
    /// (e.g. 127.0.0.1:9090). Disabled when unset.
    #[arg(long, env = "SNIPER_METRICS_ADDR")]
    metrics_addr: Option<String>,

    /// Priority fee (micro-lamports per compute unit) for swap transactions.
    #[arg(
        long,
        env = "SNIPER_PRIORITY_FEE_MICRO_LAMPORTS",
        default_value_t = 10_000
    )]
    priority_fee_micro_lamports: u64,

    /// Compute unit limit for swap transactions.
    #[arg(long, env = "SNIPER_COMPUTE_UNIT_LIMIT", default_value_t = 300_000)]
    compute_unit_limit: u32,

    /// Token candidate: pool liquidity (lamports) for strategy evaluation
    #[arg(long, default_value_t = 0)]
    pool_liquidity: u64,

    /// Token candidate: market cap (lamports) for strategy evaluation
    #[arg(long, env = "SNIPER_POOL_MARKET_CAP", default_value_t = 0)]
    pool_market_cap: u64,

    /// Token candidate: number of holders for strategy evaluation
    #[arg(long, default_value_t = 0)]
    pool_holders: u64,

    /// Token candidate: mark as blocklisted (rejects the token)
    #[arg(long, default_value_t = false)]
    pool_blocklisted: bool,

    /// Jito Block Engine endpoint. When set, live submissions go through Jito
    /// bundles with RPC fallback.
    #[arg(long, env = "SNIPER_JITO_ENDPOINT")]
    jito_endpoint: Option<String>,

    /// Jito bundle tip (lamports). Only used when --jito-endpoint is set.
    #[arg(long, env = "SNIPER_JITO_TIP_LAMPORTS", default_value_t = 0)]
    jito_tip_lamports: u64,

    /// Jito dry-run: validate the bundle payload but never POST it.
    #[arg(long, default_value_t = false)]
    jito_dry_run: bool,

    /// Raydium CLMM pool to trade on. When set, live mode resolves the pool
    /// state on-chain, feeds the real price into the strategy, and builds a
    /// real AMM swap transaction. When unset, live mode keeps the safe
    /// self-transfer test path.
    #[arg(long, env = "SNIPER_POOL_ID")]
    pool_id: Option<String>,

    /// Input token mint (base58) for the swap. Required with --pool-id.
    #[arg(long, env = "SNIPER_INPUT_MINT")]
    input_mint: Option<String>,

    /// Output token mint (base58) for the swap. Required with --pool-id.
    #[arg(long, env = "SNIPER_OUTPUT_MINT")]
    output_mint: Option<String>,

    /// Max slippage in basis points (1/100 of a percent) for the swap.
    #[arg(long, env = "SNIPER_MAX_SLIPPAGE_BPS", default_value_t = 100)]
    max_slippage_bps: u64,

    /// Max price impact (bps) of an entry: the exact, simulated output
    /// against the spot-price output. Larger entries are rejected.
    #[arg(long, env = "SNIPER_MAX_PRICE_IMPACT_BPS", default_value_t = 300)]
    max_price_impact_bps: u64,

    /// Minimum expected net edge (bps) of an entry: the take-profit target
    /// minus both legs' trading fee, price impact and network fees.
    #[arg(long, env = "SNIPER_MIN_NET_EDGE_BPS", default_value_t = 300)]
    min_net_edge_bps: i64,

    /// Exit price impact estimate, as a percentage of the measured entry
    /// impact (min 100). The exit cannot be simulated before the tokens are
    /// held, so it is costed conservatively instead of assumed equal.
    #[arg(long, env = "SNIPER_EXIT_IMPACT_PCT", default_value_t = 200)]
    exit_impact_pct: u64,

    /// Failed transactions budgeted per round trip in the net edge (each
    /// costs one network fee).
    #[arg(long, env = "SNIPER_FAILED_TX_RESERVE", default_value_t = 1)]
    failed_tx_reserve: u64,

    /// Trailing stop (bps below the best price since entry; 0 = off). Same
    /// rule in paper and live; evaluate it in paper before using it live.
    #[arg(long, env = "SNIPER_TRAILING_STOP_BPS", default_value_t = 0)]
    trailing_stop_bps: u64,

    /// Profit (bps) a position must reach before the trailing stop arms.
    #[arg(long, env = "SNIPER_TRAILING_ACTIVATION_BPS", default_value_t = 500)]
    trailing_activation_bps: u64,

    /// Paper only: adverse price move per leg (bps) charged for the delay
    /// between the decision and a real fill. Paper fills are otherwise at
    /// the observed spot price, which a live order never gets.
    #[arg(long, env = "SNIPER_PAPER_LATENCY_BPS", default_value_t = 50)]
    paper_latency_bps: u64,

    /// Max position size relative to the pool's SOL-side liquidity (bps).
    /// Unknown liquidity is a rejection.
    #[arg(long, env = "SNIPER_MAX_POSITION_LIQUIDITY_BPS", default_value_t = 200)]
    max_position_liquidity_bps: u64,

    /// Widen the exit slippage after failed exit attempts (up to 5%). Off by
    /// default: each exit attempt is quoted exactly by simulation, so a
    /// wider tolerance only buys a worse price.
    #[arg(long, env = "SNIPER_EXIT_SLIPPAGE_ESCALATION", default_value_t = false)]
    exit_slippage_escalation: bool,

    /// Signal gate on entries: EMA/MACD trend + RSI/volatility/liquidity
    /// vetoes over this process's own timestamped pool-price samples (see
    /// signal.rs). An entry waits until the indicators are warmed up.
    #[arg(long, env = "SNIPER_SIGNAL_GATE", default_value_t = true, action = clap::ArgAction::Set)]
    signal_gate: bool,

    /// Minimum signal confidence (0-100) to enter.
    #[arg(long, env = "SNIPER_SIGNAL_MIN_CONFIDENCE", default_value_t = 70)]
    signal_min_confidence: u32,

    /// Minimum spacing of the price samples (ms).
    #[arg(long, env = "SNIPER_SIGNAL_SAMPLE_MS", default_value_t = 2000)]
    signal_sample_ms: u64,

    /// The newest price sample must be at most this old (ms) for the signal
    /// gate to pass; older data is `stale_data` (no trade).
    #[arg(long, env = "SNIPER_SIGNAL_MAX_AGE_MS", default_value_t = 30_000)]
    signal_max_age_ms: u64,

    /// Max spend in SOL for a single trade (security cap).
    #[arg(long, env = "SNIPER_MAX_SPEND_SOL", default_value_t = 0.01)]
    max_spend_sol: f64,

    /// When set with --pool-id, replaces the static --pool-liquidity /
    /// --pool-holders CLI values with real on-chain data read via RPC
    /// (input vault balance + getTokenLargestAccounts/getTokenSupply).
    /// Fail-closed: an RPC error here halts the loop.
    #[arg(long, env = "SNIPER_LIVE_RISK_DATA", default_value_t = false)]
    live_risk_data: bool,

    /// Path to a local blocklist file (one base58 mint per line, `#`
    /// comments allowed). Only used with --live-risk-data. Missing file is
    /// treated as an empty blocklist, not an error.
    #[arg(long, env = "SNIPER_BLOCKLIST_FILE")]
    blocklist_file: Option<PathBuf>,

    /// Reject a candidate if the single largest holder (excluding the
    /// pool's own vault) holds more than this percent of supply. Only used
    /// with --live-risk-data.
    #[arg(long, env = "SNIPER_MAX_TOP_HOLDER_PCT", default_value_t = 50.0)]
    max_top_holder_pct: f64,

    /// Optional DexScreener cross-check for logging only. Never used to
    /// gate a trade — a DexScreener failure only logs a warning.
    #[arg(long, default_value_t = false)]
    dexscreener_check: bool,

    /// Optional WebSocket endpoint for real-time `programSubscribe` pool
    /// updates (lower latency than RPC polling). Falls back to the
    /// existing RPC-polled market data hook when unset or not yet
    /// connected.
    #[arg(long, env = "SNIPER_WS_ENDPOINT")]
    ws_endpoint: Option<String>,

    /// Manual "arm" kill switch for LIVE mode. Fail-closed default: false.
    /// The live trading loop refuses to start unless this is explicitly
    /// passed — a human must opt in every time the process starts in live
    /// mode. Has no effect in --paper or --dry-run.
    #[arg(long, env = "SNIPER_CONFIRM_LIVE", default_value_t = false)]
    confirm_live: bool,

    /// Minimum SOL-side (input vault) pool liquidity for an entry, in SOL.
    /// Floor: MIN_LIQUIDITY_FLOOR_SOL (thin pools cannot be exited).
    #[arg(long, env = "SNIPER_MIN_LIQUIDITY_SOL", default_value_t = 1000.0)]
    min_liquidity_sol: f64,

    /// Stop after the first completed trade: a closed position (paper /
    /// live) or one built+simulated entry (dry-run). Used by the pool
    /// discovery supervisor, which runs one process per new pool.
    #[arg(long, env = "SNIPER_SINGLE_TRADE", default_value_t = false)]
    single_trade: bool,

    /// Stop when no entry happened within this many seconds of startup
    /// (0 = no limit). An open position is always managed to its exit.
    #[arg(long, env = "SNIPER_ENTRY_WINDOW_SECS", default_value_t = 0)]
    entry_window_secs: u64,
}

impl Args {
    /// Treat empty values as unset: `SNIPER_POOL_ID=` in an env file (as in
    /// .env.example) must mean "not configured", not an empty pool id.
    fn normalized(mut self) -> Self {
        fn s(v: &mut Option<String>) {
            if v.as_deref().is_some_and(|x| x.trim().is_empty()) {
                *v = None;
            }
        }
        fn p(v: &mut Option<PathBuf>) {
            if v.as_ref().is_some_and(|x| x.as_os_str().is_empty()) {
                *v = None;
            }
        }
        s(&mut self.blockhash);
        s(&mut self.hsm_endpoint);
        p(&mut self.hsm_ca);
        p(&mut self.hsm_client_identity);
        s(&mut self.expected_signer);
        s(&mut self.simulate_payer);
        s(&mut self.metrics_addr);
        s(&mut self.jito_endpoint);
        s(&mut self.pool_id);
        s(&mut self.input_mint);
        s(&mut self.output_mint);
        p(&mut self.blocklist_file);
        s(&mut self.ws_endpoint);
        self
    }
}

/// Initialize tracing with JSON structured logging + file rotation.
fn init_tracing(data_dir: &std::path::Path) -> tracing_appender::non_blocking::WorkerGuard {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,solana_sniper=debug"));

    let log_dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let file_appender = tracing_appender::rolling::daily(&log_dir, "hft.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .json()
        // File (daily rotation) AND stdout, so `journalctl -u solana-hft`
        // shows the same JSON lines under systemd. Every line is redacted:
        // RPC URLs carry provider API keys (also inside RPC error messages).
        .with_writer(redact::RedactingMakeWriter::new(
            non_blocking.and(std::io::stdout),
        ))
        .init();

    tracing::info!(target: "main", log_dir = %log_dir.display(), "logging initialized (JSON + daily rotation)");
    guard
}

fn load_keypair(path: &PathBuf) -> Result<Keypair, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(path)?;
    let bytes: Vec<u8> = serde_json::from_str(&data)?;
    let arr: [u8; 64] = bytes
        .try_into()
        .map_err(|_| "wallet.json must be exactly 64 bytes")?;
    Keypair::try_from(&arr[..]).map_err(|e| e.into())
}

/// Fail-closed signing configuration validation.
///
/// - `--live` and `--dry-run` are mutually exclusive.
/// - LIVE mode requires the remote HSM; the local keyfile is never a fallback.
/// - Any HSM endpoint requires mTLS (`--hsm-ca` + `--hsm-client-identity`).
fn validate_signing_config(
    live: bool,
    dry_run: bool,
    paper: bool,
    confirm_live: bool,
    hsm_endpoint: &Option<String>,
    hsm_ca: &Option<PathBuf>,
    hsm_client_identity: &Option<PathBuf>,
) -> Result<(), String> {
    if live && dry_run {
        return Err("--live and --dry-run are mutually exclusive".to_string());
    }
    if paper && (live || dry_run) {
        return Err("--paper is mutually exclusive with --live and --dry-run".to_string());
    }
    if live && !confirm_live {
        return Err(
            "LIVE mode requires the manual arm switch --confirm-live. Fail-closed: the live loop never starts by default."
                .to_string(),
        );
    }
    if live && hsm_endpoint.is_none() {
        return Err(
            "LIVE mode requires --hsm-endpoint (remote HSM). Local keyfile signing is disabled in live mode (fail-closed)."
                .to_string(),
        );
    }
    if hsm_endpoint.is_some() && (hsm_ca.is_none() || hsm_client_identity.is_none()) {
        return Err(
            "--hsm-endpoint requires both --hsm-ca and --hsm-client-identity (mTLS is mandatory)."
                .to_string(),
        );
    }
    Ok(())
}

/// Fetch the HSM signer's public key. The blocking reqwest client builds its
/// own internal tokio runtime, so it must be created AND dropped on the
/// blocking thread pool (spawn_blocking) to avoid dropping a runtime inside
/// the async runtime (which panics).
async fn hsm_pubkey(
    endpoint: &str,
    ca: &Path,
    identity: &Path,
) -> Result<Pubkey, Box<dyn std::error::Error>> {
    let endpoint = endpoint.to_string();
    let ca = ca.to_path_buf();
    let identity = identity.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Pubkey, String> {
        let signer = RemoteHsmSigner::new(&endpoint, Some(&ca), Some(&identity))
            .map_err(|e| format!("{e}"))?;
        signer.pubkey()
    })
    .await
    .map_err(|e| format!("hsm pubkey task failed: {e}"))?
    .map_err(|e| e.into())
}

/// Sign a transaction via the remote HSM. Same blocking-runtime discipline as
/// `hsm_pubkey`: the client is created and dropped inside spawn_blocking.
async fn hsm_sign(
    endpoint: &str,
    ca: &Path,
    identity: &Path,
    tx: &mut Transaction,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let endpoint = endpoint.to_string();
    let ca = ca.to_path_buf();
    let identity = identity.to_path_buf();
    let tx = tx.clone();
    tokio::task::spawn_blocking(move || -> Result<Signature, String> {
        let signer = RemoteHsmSigner::new(&endpoint, Some(&ca), Some(&identity))
            .map_err(|e| format!("{e}"))?;
        let mut t = tx;
        signer.sign_transaction(&mut t)
    })
    .await
    .map_err(|e| format!("hsm signing task failed: {e}"))?
    .map_err(|e| e.into())
}

/// Resolve the blockhash to use: an explicit `--blockhash` (dry-run, offline)
/// or a fresh one from the RPC.
fn resolve_blockhash(
    args: &Args,
    blockhash_mgr: &std::sync::Mutex<retry::BlockhashManager>,
) -> Result<solana_sdk::hash::Hash, Box<dyn std::error::Error>> {
    if let Some(bh) = &args.blockhash {
        Ok(
            solana_sdk::hash::Hash::from_str(bh)
                .map_err(|e| format!("invalid --blockhash: {e}"))?,
        )
    } else {
        blockhash_mgr.lock().unwrap().force_refresh()
    }
}

/// AŞAMA 2/5 — paper mode runs on REAL on-chain market data, so it requires
/// the same pool/mint arguments as live. Fail-closed: without them paper
/// refuses to start (no synthetic paper pools anymore).
fn validate_paper_args(
    pool_id: Option<&str>,
    input_mint: Option<&str>,
    output_mint: Option<&str>,
) -> Result<(), String> {
    match (pool_id, input_mint, output_mint) {
        (None, _, _) => Err(
            "--paper requires --pool-id (real on-chain pool data — no synthetic pools). \
             Example: --paper --pool-id <devnet CLMM pool> --input-mint ... --output-mint ..."
                .to_string(),
        ),
        (_, None, _) | (_, _, None) => Err(
            "--paper requires --input-mint and --output-mint together with --pool-id".to_string(),
        ),
        (Some(_), Some(_), Some(_)) => Ok(()),
    }
}

/// LIVE trading prerequisites beyond signing (fail-closed): a real pool to
/// trade (the self-transfer test path is dry-run only), real on-chain risk
/// data (static CLI liquidity/holder values are never trusted with real
/// funds), and a pinned HSM wallet key.
fn validate_live_args(
    live: bool,
    pool_id: Option<&str>,
    input_mint: Option<&str>,
    output_mint: Option<&str>,
    live_risk_data: bool,
    expected_signer: Option<&str>,
) -> Result<Option<Pubkey>, String> {
    let pinned = match expected_signer {
        Some(s) => {
            Some(Pubkey::from_str(s).map_err(|e| format!("invalid --expected-signer: {e}"))?)
        }
        None => None,
    };
    if !live {
        return Ok(pinned);
    }
    if pool_id.is_none() || input_mint.is_none() || output_mint.is_none() {
        return Err(
            "LIVE mode requires --pool-id, --input-mint and --output-mint (a real pool to trade)."
                .to_string(),
        );
    }
    if !live_risk_data {
        return Err(
            "LIVE mode requires --live-risk-data (real on-chain liquidity + holder data; static CLI values are never trusted with real funds)."
                .to_string(),
        );
    }
    if pinned.is_none() {
        return Err(
            "LIVE mode requires --expected-signer <wallet pubkey> so the HSM key is validated before any trade (fail-closed)."
                .to_string(),
        );
    }
    Ok(pinned)
}

/// SOL kept in the wallet on top of the trade size: rent for a new token
/// account (~0.00204) and the temporary wSOL account (~0.00204, refunded in
/// the same tx), plus network/priority fees for the entry and the exit.
const SOL_RESERVE_LAMPORTS: u64 = 6_000_000;

/// SOL → lamports (truncating, like the deprecated solana_sdk helper).
fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1_000_000_000f64) as u64
}

/// Lowest accepted --min-liquidity-sol: below it a pool is too thin to
/// exit a position at a sane price.
const MIN_LIQUIDITY_FLOOR_SOL: f64 = 20.0;

/// Strategy configuration for this run: the fail-closed defaults with the
/// operator's liquidity floor.
fn strategy_config(args: &Args) -> strategy::StrategyConfig {
    strategy::StrategyConfig {
        min_liquidity_lamports: sol_to_lamports(args.min_liquidity_sol),
        trailing_stop_bps: args.trailing_stop_bps,
        trailing_activation_bps: args.trailing_activation_bps,
        ..strategy::StrategyConfig::default()
    }
}

/// The trailing stop, when on, must be a sane band (< 50 %) that arms at a
/// profit (> 0): an unarmed trailing stop could fire at a loss.
fn validate_trailing_stop(trailing_bps: u64, activation_bps: u64) -> Result<(), String> {
    if trailing_bps == 0 {
        return Ok(());
    }
    if trailing_bps >= 5_000 {
        return Err(format!(
            "--trailing-stop-bps must be < 5000 (got {trailing_bps})"
        ));
    }
    if activation_bps == 0 {
        return Err("--trailing-activation-bps must be > 0 when the trailing stop is on".into());
    }
    Ok(())
}

fn validate_min_liquidity(min_liquidity_sol: f64) -> Result<(), String> {
    if !min_liquidity_sol.is_finite() || min_liquidity_sol < MIN_LIQUIDITY_FLOOR_SOL {
        return Err(format!(
            "--min-liquidity-sol must be >= {MIN_LIQUIDITY_FLOOR_SOL} SOL (got {min_liquidity_sol})"
        ));
    }
    Ok(())
}

/// True when the run should stop because no entry happened within the
/// entry window (never while a position is open).
fn entry_window_expired(elapsed_secs: u64, window_secs: u64, position_open: bool) -> bool {
    window_secs > 0 && !position_open && elapsed_secs >= window_secs
}

fn native_mint() -> Pubkey {
    Pubkey::from_str(amm::raydium_v4::NATIVE_MINT).expect("valid native mint")
}

/// Upper bound for the escalated exit slippage (5%).
const EXIT_MAX_SLIPPAGE_BPS: u64 = 500;

/// Slippage for an exit attempt: the configured slippage, widened by one
/// configured step per consecutive failed exit, capped at 5%. In a fast
/// falling market a stop-loss sell at the normal 1% tolerance can fail
/// (preflight rejects it, no fee is paid) iteration after iteration while
/// the loss grows; widening guarantees the position is eventually closed.
fn exit_slippage_bps(base_bps: u64, consecutive_failures: u32) -> u64 {
    base_bps
        .saturating_mul(1 + u64::from(consecutive_failures))
        .min(EXIT_MAX_SLIPPAGE_BPS.max(base_bps))
}

/// True when a position opened at `opened_at_ms` has reached `max_hold_secs`
/// (0 = no limit).
fn max_hold_reached(opened_at_ms: u64, now_ms: u64, max_hold_secs: u64) -> bool {
    max_hold_secs > 0 && now_ms.saturating_sub(opened_at_ms) >= max_hold_secs.saturating_mul(1000)
}

/// Minimum input-side balance required before an entry is sent.
fn entry_required_balance(position_size: u64, input_is_native: bool) -> u64 {
    if input_is_native {
        position_size.saturating_add(SOL_RESERVE_LAMPORTS)
    } else {
        position_size
    }
}

/// Build the exit (sell) transaction for `amount` raw units of the output
/// mint back into the input mint — the same primitives the live exit path
/// (`manage_open_position`) uses: reversed account resolution, real pool
/// fee and tick arrays, ATA setup, wSOL unwrap.
fn build_exit_tx(
    args: &Args,
    rpc: &RpcClient,
    from: &Pubkey,
    amount: u64,
    blockhash: solana_sdk::hash::Hash,
    jito_tip: Option<(Pubkey, u64)>,
) -> Result<Transaction, String> {
    let parse = |label: &str, v: Option<&str>| {
        Pubkey::from_str(v.unwrap_or_default()).map_err(|e| format!("invalid {label}: {e}"))
    };
    let pool_id = parse("--pool-id", args.pool_id.as_deref())?;
    let back_mint = parse("--input-mint", args.input_mint.as_deref())?;
    let held_mint = parse("--output-mint", args.output_mint.as_deref())?;
    if amount == 0 {
        return Err("entry quote produced no output to sell".into());
    }
    let program_id = clmm_program_id(&args.rpc);
    let (accounts, pool) = amm::account_resolver::resolve_swap_accounts(
        rpc,
        &pool_id,
        from,
        &held_mint,
        &back_mint,
        &program_id,
    )?;
    let adapter = amm::raydium_v4::RaydiumV4ClmmAdapter::new(pool_id.to_string())
        .with_program_id(program_id.to_string())
        .with_swap_accounts(accounts)
        .with_resolved_pool(pool);
    let quote = adapter
        .quote(amount, args.max_slippage_bps)
        .map_err(|e| format!("exit quote failed: {e}"))?;
    let intent = adapter
        .build_intent(quote)
        .map_err(|e| format!("exit build_intent failed: {e}"))?;
    let opts = amm::raydium_v4::SwapTxOptions {
        input_mint: held_mint,
        output_mint: back_mint,
        compute_unit_limit: args.compute_unit_limit,
        priority_fee_micro_lamports: args.priority_fee_micro_lamports,
        close_input_token_account: false,
        jito_tip,
        keep_output_wrapped: false,
    };
    adapter
        .build_swap_transaction(&intent, from, blockhash, &opts)
        .map_err(|e| format!("exit build_transaction failed: {e}"))
}

/// The signal gate's rejection code, or `None` when the entry may proceed
/// (or the gate is disabled).
fn signal_gate_rejects(
    args: &Args,
    series: &signal::Series,
    cfg: &signal::SignalConfig,
) -> Option<&'static str> {
    if !args.signal_gate {
        return None;
    }
    match signal::evaluate_at(series, cfg, marketdata::now_ms() as u64) {
        signal::SignalDecision::Trade { .. } => None,
        signal::SignalDecision::NoTrade { reason, .. } => Some(reason),
    }
}

/// The signal score (0-100) at this moment, when the gate would trade.
fn signal_confidence(series: &signal::Series, cfg: &signal::SignalConfig) -> Option<u32> {
    match signal::evaluate_at(series, cfg, marketdata::now_ms() as u64) {
        signal::SignalDecision::Trade { confidence } => Some(confidence),
        signal::SignalDecision::NoTrade { .. } => None,
    }
}

/// Exact quote of a swap: the spot-price quote (reference for the price
/// impact) and the output the deployed program actually delivers for this
/// very swap, from an on-chain simulation (see `sim_quote`). Fail-closed:
/// no simulated output, no quote.
fn exact_quote(
    rpc: &RpcClient,
    adapter: &amm::raydium_v4::RaydiumV4ClmmAdapter,
    amount: u64,
    slippage_bps: u64,
    from: &Pubkey,
    opts: &amm::raydium_v4::SwapTxOptions,
) -> Result<(amm::Quote, sim_quote::SimQuote), String> {
    let spot = adapter
        .quote(amount, slippage_bps)
        .map_err(|e| format!("spot quote failed: {e}"))?;
    let output_account = adapter
        .output_token_account()
        .ok_or("swap accounts not resolved")?;
    let pre_balance = position::fetch_token_balance(rpc, &output_account)?;
    let quote_tx = adapter
        .build_quote_transaction(&spot, from, solana_sdk::hash::Hash::default(), opts)
        .map_err(|e| format!("quote transaction build failed: {e}"))?;
    let sim = sim_quote::simulate_swap_output(rpc, &quote_tx, &output_account, pre_balance)?;
    Ok((spot, sim))
}

/// Entry economics gate on an exact quote: price impact, position size
/// against the pool's SOL-side liquidity, and the expected net edge after
/// both legs' fees, the measured entry impact, a conservative exit-impact
/// estimate, network fees and a failed-transaction reserve. `Err(code)`
/// names the rejection; unknown inputs reject.
fn entry_economics(
    args: &Args,
    spot_out: u64,
    exact_out: u64,
    fee_ppm: u64,
    position_lamports: u64,
    jito_tip_lamports: u64,
    liquidity_lamports: Option<u64>,
) -> Result<sim_quote::EdgeBreakdown, &'static str> {
    let impact = sim_quote::price_impact_bps(spot_out, exact_out);
    if impact > args.max_price_impact_bps {
        return Err("price_impact_exceeded");
    }
    let ratio =
        sim_quote::position_liquidity_bps(position_lamports, liquidity_lamports.unwrap_or(0))
            .ok_or("liquidity_unverified")?;
    if ratio > args.max_position_liquidity_bps {
        return Err("position_too_large_for_liquidity");
    }
    let network = sim_quote::tx_network_cost_lamports(
        args.compute_unit_limit,
        args.priority_fee_micro_lamports,
    )
    .saturating_add(jito_tip_lamports);
    let edge = sim_quote::edge_breakdown(&sim_quote::EdgeInputs {
        take_profit_bps: strategy_config(args).take_profit_bps,
        fee_rate_ppm: fee_ppm,
        entry_impact_bps: impact,
        exit_impact_pct: args.exit_impact_pct,
        network_cost_per_tx_lamports: network,
        failed_tx_reserve: args.failed_tx_reserve,
        exit_slippage_bps: args.max_slippage_bps,
        position_lamports,
    })
    .ok_or("net_edge_too_small")?;
    if edge.net_edge_bps < args.min_net_edge_bps {
        return Err("net_edge_too_small");
    }
    Ok(edge)
}

/// Consecutive failed exit attempts after which the position is reported
/// as `exit_blocked` (retries continue).
const EXIT_BLOCKED_AFTER: u32 = 3;

/// `exit_blocked` readiness flag (alert + live /ready 503): an open live
/// position whose last `EXIT_BLOCKED_AFTER` exit attempts all failed.
fn exit_blocked(position_open: bool, consecutive_exit_failures: u32) -> bool {
    position_open && consecutive_exit_failures >= EXIT_BLOCKED_AFTER
}

/// Result of an RPC `simulateTransaction`.
struct SimulationResult {
    err: Option<String>,
    units: Option<u64>,
    logs: Vec<String>,
}

/// Simulate a (possibly unsigned) transaction against the current chain
/// state with signature verification off and a fresh blockhash. Read-only:
/// nothing is sent.
fn simulate_on_chain(rpc: &RpcClient, tx: &Transaction) -> Result<SimulationResult, String> {
    let cfg = solana_rpc_client_api::config::RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        ..Default::default()
    };
    let resp = tokio::task::block_in_place(|| {
        rpc.simulate_transaction_with_config(tx, cfg)
            .map_err(|e| format!("simulateTransaction failed: {e}"))
    })?;
    let v = resp.value;
    Ok(SimulationResult {
        err: v.err.map(|e| format!("{e:?}")),
        units: v.units_consumed,
        logs: v.logs.unwrap_or_default(),
    })
}

/// Raydium CLMM program id for the cluster the RPC points at.
fn clmm_program_id(rpc: &str) -> Pubkey {
    if rpc.contains("devnet") {
        Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID_DEVNET)
            .expect("valid devnet program id")
    } else {
        Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID)
            .expect("valid mainnet program id")
    }
}

/// Sign `tx` (remote HSM when configured, else the dry-run local keypair) and
/// verify the signature before it is attached. Never returns Ok for an
/// unverifiable signature.
async fn sign_and_verify(
    args: &Args,
    local_signer: Option<&Keypair>,
    from: &Pubkey,
    tx: &mut Transaction,
    metrics_registry: &metrics::Metrics,
) -> Result<(), String> {
    let sig = if let (Some(ep), Some(ca), Some(id)) = (
        args.hsm_endpoint.as_ref(),
        args.hsm_ca.as_ref(),
        args.hsm_client_identity.as_ref(),
    ) {
        let start = std::time::Instant::now();
        let res = hsm_sign(ep, ca, id, tx).await.map_err(|e| format!("{e}"));
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::record_hsm(metrics_registry, res.is_ok(), ms);
        metrics::observe_stage(metrics_registry, metrics::Stage::Sign, ms);
        res?
    } else if let Some(kp) = local_signer {
        tx.sign(&[kp], tx.message.recent_blockhash);
        tx.signatures[0]
    } else {
        return Err("no signer available at signing time (fail-closed)".to_string());
    };
    remote_hsm::verify_hsm_signature(tx, from, &sig)?;
    tx.signatures = vec![sig];
    Ok(())
}

/// Submit a signed transaction in LIVE mode and wait for confirmation: via a
/// Jito bundle (with RPC fallback) when configured, otherwise via RPC. A
/// bundle that was merely accepted is NOT reported as success until the
/// signature is confirmed on-chain.
async fn submit_live_tx(
    args: &Args,
    rpc_client: &RpcClient,
    tx: &Transaction,
) -> Result<Signature, Box<dyn std::error::Error>> {
    // Last line of defence: nothing is ever broadcast outside an armed live
    // run, whatever path reached this function.
    if !args.live || !args.confirm_live || args.paper || args.dry_run {
        return Err("broadcast refused: not an armed live run (fail-closed)".into());
    }
    let Some(jito_ep) = &args.jito_endpoint else {
        return retry::send_with_retry(rpc_client, tx);
    };
    let bundle = jito::JitoBundle::new(vec![tx.clone()], args.jito_tip_lamports);
    let client = jito::JitoClient::new(jito_ep, args.jito_dry_run);
    if args.jito_dry_run {
        match client.send_bundle(&bundle).await {
            Ok(bundle_id) => {
                tracing::info!(bundle_id = %bundle_id, "jito bundle dry-run validated — submitting via RPC")
            }
            Err(e) => {
                tracing::warn!(error = %e, "jito dry-run validation failed — submitting via RPC")
            }
        }
        return retry::send_with_retry(rpc_client, tx);
    }
    match client.send_bundle(&bundle).await {
        Ok(bundle_id) => {
            tracing::info!(bundle_id = %bundle_id, "jito bundle accepted — confirming on-chain");
            let sig = tx.signatures[0];
            match retry::confirm_transaction(rpc_client, &sig, &tx.message.recent_blockhash) {
                Ok(()) => Ok(sig),
                Err(e) => {
                    tracing::warn!(error = %e, "jito bundle not confirmed — falling back to RPC");
                    retry::send_with_retry(rpc_client, tx)
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "jito bundle failed — falling back to RPC");
            jito::send_with_rpc_fallback(rpc_client, std::slice::from_ref(tx)).await
        }
    }
}

/// Book (or drop) an entry that was sent but whose outcome was never
/// recorded — the process died while confirming it. Waits until the
/// transaction is confirmed or its blockhash expired, then decides from the
/// real balances. Fail-closed: if the outcome cannot be established, LIVE
/// does not start.
fn reconcile_pending_entry(
    data_dir: &Path,
    rpc: &RpcClient,
    risk_manager: &risk::RiskManager,
) -> Result<(), String> {
    let Some(p) = position::PendingEntry::load(data_dir)
        .map_err(|e| format!("pending entry record unreadable — refusing to arm LIVE: {e}"))?
    else {
        return Ok(());
    };
    if position::LivePosition::load(data_dir)?.is_some() {
        // The position was booked; only the cleanup was interrupted.
        return position::PendingEntry::clear(data_dir);
    }
    tracing::warn!(target: "main", signature = %p.signature, pool = %p.position.pool_id, "unfinished entry from a previous run — reconciling from the chain");
    let sig = Signature::from_str(&p.signature).map_err(|e| format!("pending entry: {e}"))?;
    let bh = solana_sdk::hash::Hash::from_str(&p.blockhash)
        .map_err(|e| format!("pending entry blockhash: {e}"))?;
    // Ok = landed; Err = failed / expired / unknown — the balances decide.
    let confirm = retry::confirm_transaction(rpc, &sig, &bh);
    let parse = |v: &str| Pubkey::from_str(v).map_err(|e| format!("pending entry mint: {e}"));
    let owner_ata_mints = (
        parse(&p.position.input_mint)?,
        parse(&p.position.output_mint)?,
    );
    let owner = rpc
        .get_transaction_with_config(
            &sig,
            solana_rpc_client_api::config::RpcTransactionConfig {
                encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        )
        .ok()
        .and_then(|t| t.transaction.transaction.decode())
        .and_then(|t| t.message.static_account_keys().first().copied());
    let Some(owner) = owner else {
        if confirm.is_err() {
            // Not on chain: the swap never happened.
            tracing::warn!(target: "main", signature = %p.signature, "unfinished entry never landed — dropped");
            return position::PendingEntry::clear(data_dir);
        }
        return Err(format!(
            "unfinished entry {} confirmed but not readable — inspect the wallet before arming LIVE (fail-closed)",
            p.signature
        ));
    };
    let after_in = position::fetch_mint_balance(rpc, &owner, &owner_ata_mints.0)?;
    let after_out = position::fetch_mint_balance(rpc, &owner, &owner_ata_mints.1)?;
    match p.resolve(after_in, after_out) {
        Some(pos) => {
            pos.save(data_dir)?;
            risk_manager.record_trade();
            risk_manager.record_position_open(pos.position_size_lamports);
            tracing::warn!(target: "main", signature = %p.signature, tokens = pos.token_amount, cost = pos.cost_input_units, "unfinished entry LANDED — position recorded and will be managed");
        }
        None => {
            tracing::warn!(target: "main", signature = %p.signature, "unfinished entry did not deliver tokens — dropped");
        }
    }
    position::PendingEntry::clear(data_dir)
}

/// Outcome of one exit-management iteration for the open live position.
#[derive(Debug)]
enum ExitOutcome {
    /// Position held (no stop-loss / take-profit signal).
    Hold,
    /// Position sold on-chain; realized P&L in input units.
    Closed {
        decision: strategy::ExitDecision,
        pnl: i64,
    },
    /// Position tokens are no longer in the wallet (sold/moved externally or a
    /// previous exit landed before the record was cleared): bookkeeping closed.
    ClosedExternally,
    /// Exit could not be evaluated or executed this iteration; the position
    /// stays open and is retried next iteration.
    Failed(String),
}

/// Manage the open live position: read the real pool state, value the held
/// tokens with a reverse-direction quote, and SELL them (reverse swap) on a
/// stop-loss or take-profit signal. Exits intentionally bypass the entry
/// risk gates (kill switch / circuit breaker / daily caps): closing a
/// position only ever reduces risk, so a tripped breaker must never trap
/// capital in an open position.
#[allow(clippy::too_many_arguments)]
async fn manage_open_position(
    args: &Args,
    rpc_client: &Arc<RpcClient>,
    risk_manager: &risk::RiskManager,
    metrics_registry: &metrics::Metrics,
    blockhash_mgr: &std::sync::Mutex<retry::BlockhashManager>,
    from: &Pubkey,
    pos: &mut position::LivePosition,
    jito_tip: Option<(Pubkey, u64)>,
    exit_failures: u32,
) -> ExitOutcome {
    let parse = |label: &str, v: &str| {
        Pubkey::from_str(v).map_err(|e| format!("invalid {label} in live position: {e}"))
    };
    let (pool_id, held_mint, back_mint) = match (
        parse("pool_id", &pos.pool_id),
        parse("output_mint", &pos.output_mint),
        parse("input_mint", &pos.input_mint),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return ExitOutcome::Failed(e),
    };
    let program_id = clmm_program_id(&args.rpc);

    // Reverse direction: sell the held token back into the entry mint. The
    // pool state is fetched fresh from the RPC here (exit correctness over
    // latency), so no staleness window applies.
    let (accounts, pool) = match amm::account_resolver::resolve_swap_accounts(
        rpc_client,
        &pool_id,
        from,
        &held_mint,
        &back_mint,
        &program_id,
    ) {
        Ok(v) => v,
        Err(e) => return ExitOutcome::Failed(format!("exit account resolution failed: {e}")),
    };
    let held_ata = accounts.input_token_account;
    let adapter_output_vault = accounts.output_vault;
    let held = match position::fetch_token_balance(rpc_client, &held_ata) {
        Ok(v) => v,
        Err(e) => return ExitOutcome::Failed(e),
    };
    if held == 0 {
        return ExitOutcome::ClosedExternally;
    }
    // Never sell more than this position bought (other holdings untouched).
    let amount = held.min(pos.token_amount.max(1));

    let adapter = amm::raydium_v4::RaydiumV4ClmmAdapter::new(pos.pool_id.clone())
        .with_program_id(program_id.to_string())
        .with_swap_accounts(accounts)
        .with_resolved_pool(pool);
    let slippage = if args.exit_slippage_escalation {
        exit_slippage_bps(args.max_slippage_bps, exit_failures)
    } else {
        args.max_slippage_bps
    };
    let opts = amm::raydium_v4::SwapTxOptions {
        input_mint: held_mint,
        output_mint: back_mint,
        compute_unit_limit: args.compute_unit_limit,
        priority_fee_micro_lamports: args.priority_fee_micro_lamports,
        // Selling the whole balance empties the ATA: close it so its rent
        // returns to the wallet and shows up in the realized P&L.
        close_input_token_account: amount == held,
        jito_tip,
        keep_output_wrapped: false,
    };
    // The position is valued at what selling it would REALLY return (an
    // on-chain simulation of this exit: price impact included). If the
    // simulation is unavailable, the spot quote is used — its minimum
    // output is stricter, so an exit never executes at a worse price.
    let quote = match exact_quote(rpc_client, &adapter, amount, slippage, from, &opts) {
        Ok((mut spot, sim)) => {
            spot.expected_output = sim.amount_out;
            spot
        }
        Err(e) => {
            tracing::warn!(target: "live", error = %e, "exact exit quote unavailable — valuing at spot price");
            match adapter.quote(amount, slippage) {
                Ok(q) => q,
                Err(e) => return ExitOutcome::Failed(format!("exit quote failed: {e}")),
            }
        }
    };
    // The exit signal compares against the amount actually swapped in (the
    // entry is exact-input), not the measured cost: the measured cost also
    // contains the token-account rent, which is refunded on a full exit and
    // would otherwise fire a false stop-loss right after every entry. The
    // measured cost is still what realized P&L is computed from.
    let strategy = strategy::SimpleSnipeStrategy::new(strategy_config(args));
    let swapped_in = if amount < pos.token_amount {
        // Partial balance (some tokens left the wallet): pro-rate.
        (pos.position_size_lamports as u128 * amount as u128 / pos.token_amount as u128) as u64
    } else {
        pos.position_size_lamports
    };
    // Same integer ratio and exit rule as paper (exit_with_peak). The best
    // ratio since entry is persisted so the trailing stop survives restarts.
    let mut decision = match strategy::value_ratio_scaled(swapped_in, quote.expected_output) {
        Some(ratio) => {
            let ratio64 = u64::try_from(ratio).unwrap_or(u64::MAX);
            if ratio64 > pos.peak_value_ratio {
                pos.peak_value_ratio = ratio64;
                if let Err(e) = pos.save(&args.data_dir) {
                    tracing::warn!(target: "live", error = %e, "could not persist the position's best price (trailing stop uses the in-memory value)");
                }
            }
            strategy.exit_with_peak(ratio, pos.peak_value_ratio as u128)
        }
        None => strategy::ExitDecision::Hold,
    };
    // Liquidity being pulled (rug): both vaults shrink while the price may
    // barely move, so the price exits never fire. The exit swap's output
    // vault is the entry-mint (SOL) side.
    let sol_liquidity = if pos.entry_liquidity_lamports > 0 {
        match onchain_risk::fetch_vault_liquidity(rpc_client, &adapter_output_vault) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(target: "live", error = %e, "liquidity read failed — liquidity-drop exit skipped this iteration");
                None
            }
        }
    } else {
        None
    };
    if decision == strategy::ExitDecision::Hold
        && sol_liquidity
            .is_some_and(|now| strategy::liquidity_dropped(pos.entry_liquidity_lamports, now))
    {
        decision = strategy::ExitDecision::LiquidityDrop;
    }
    if decision == strategy::ExitDecision::Hold
        && max_hold_reached(
            pos.opened_at_ms,
            marketdata::now_ms() as u64,
            args.max_hold_secs,
        )
    {
        decision = strategy::ExitDecision::MaxHoldTime;
    }
    tracing::info!(
        target: "live",
        pool_id = %pos.pool_id,
        exit_slippage_bps = slippage,
        swapped_in,
        cost = pos.cost_input_units,
        current_value = quote.expected_output,
        sol_liquidity,
        entry_liquidity = pos.entry_liquidity_lamports,
        decision = ?decision,
        "open position valued on real pool state"
    );
    if decision == strategy::ExitDecision::Hold {
        return ExitOutcome::Hold;
    }

    let intent = match adapter.build_intent(quote) {
        Ok(i) => i,
        Err(e) => return ExitOutcome::Failed(format!("exit build_intent failed: {e}")),
    };
    let blockhash = match blockhash_mgr.lock().unwrap().force_refresh() {
        Ok(b) => b,
        Err(e) => return ExitOutcome::Failed(format!("exit blockhash fetch failed: {e}")),
    };
    let mut tx = match adapter.build_swap_transaction(&intent, from, blockhash, &opts) {
        Ok(t) => t,
        Err(e) => return ExitOutcome::Failed(format!("exit build_transaction failed: {e}")),
    };
    if let Err(e) = sign_and_verify(args, None, from, &mut tx, metrics_registry).await {
        return ExitOutcome::Failed(format!("exit signing failed: {e}"));
    }

    let before = match position::fetch_mint_balance(rpc_client, from, &back_mint) {
        Ok(v) => v,
        Err(e) => return ExitOutcome::Failed(e),
    };
    // As with entries, an Err may mean "outcome unknown": if the held token
    // balance dropped, the sell landed — book it with its real proceeds
    // instead of retrying and later mis-reporting it as an external close.
    let submitted = match submit_live_tx(args, rpc_client, &tx).await {
        Ok(sig) => Ok(sig),
        Err(e) => match position::fetch_token_balance(rpc_client, &held_ata) {
            Ok(now_held) if now_held < held => {
                tracing::warn!(target: "live", error = %e, "exit submission reported an error but the sell LANDED");
                Ok(tx.signatures[0])
            }
            _ => Err(e),
        },
    };
    match submitted {
        Ok(sig) => {
            let proceeds = position::fetch_mint_balance(rpc_client, from, &back_mint)
                .map(|after| after.saturating_sub(before))
                .unwrap_or(intent.min_output);
            let pnl = position::realized_pnl(pos.cost_input_units, proceeds);
            tracing::info!(
                target: "live",
                signature = %sig,
                decision = ?decision,
                proceeds,
                pnl,
                "EXIT confirmed on-chain"
            );
            println!("[LIVE] EXIT {decision:?} confirmed: {sig} (realized pnl {pnl})");
            if pnl < 0 {
                let _ = risk_manager.record_loss(pnl.unsigned_abs());
            }
            ExitOutcome::Closed { decision, pnl }
        }
        Err(e) => ExitOutcome::Failed(format!("exit transaction failed: {e}")),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Optional `.env` in the working directory: every CLI flag can also be
    // set through its SNIPER_* environment variable (see .env.example). An
    // explicit CLI flag always wins over the environment.
    let _ = dotenvy::dotenv();
    let args = Args::parse().normalized();
    std::fs::create_dir_all(&args.data_dir)?;
    let _guard = init_tracing(&args.data_dir);

    tracing::info!(
        target: "main",
        rpc = %args.rpc,
        ws = %args.ws,
        dry_run = args.dry_run,
        live = args.live,
        "Solana HFT platform starting"
    );

    // Centralized, fail-closed risk defaults (0.05 SOL/trade, 5 trades/day,
    // 0.20 SOL daily loss kill-switch, 1 open position, 2% max slippage).
    // Applied to every mode (paper/dry-run/live) for consistency — if the
    // config itself cannot be validated, refuse to start at all.
    validate_min_liquidity(args.min_liquidity_sol)?;
    validate_trailing_stop(args.trailing_stop_bps, args.trailing_activation_bps)?;
    let risk_cfg = risk::RiskConfig::production_defaults(args.data_dir.clone())?;
    // Fail-closed cross-module invariant: the strategy's per-trade position
    // size must never exceed the risk manager's max_trade_size_lamports,
    // otherwise every entry dies at the pre_trade_check gate with
    // MaxSpendExceeded (or the cap is silently bypassed). Both the paper and
    // live paths construct SimpleSnipeStrategy from StrategyConfig::default()
    // and pass signals through the SAME pre_trade_check, so one startup
    // check covers every mode. Refuse to start on violation.
    strategy::validate_position_size_invariant(
        strategy::StrategyConfig::default().max_trade_size_lamports,
        risk_cfg.max_trade_size_lamports,
    )?;
    // One live process per data dir, taken BEFORE the risk state is read so
    // no other process can change it underneath this one.
    let _instance_lock = if args.live {
        Some(position::InstanceLock::acquire(&args.data_dir)?)
    } else {
        None
    };
    let risk_manager = Arc::new(risk::RiskManager::new(risk_cfg.clone()));
    tracing::info!(
        target: "main",
        daily_loss = risk_manager.current_daily_loss(),
        circuit_breaker = risk_manager.is_circuit_breaker_active(),
        state_verified = risk_manager.is_state_verified(),
        "risk manager initialized"
    );

    if args.live {
        // Fail-closed restart safety: if the persisted risk state
        // (risk_state.json) could not be parsed, we cannot trust daily
        // counters or open-position accounting after a restart. Refuse to
        // arm live trading; the operator must investigate and clear the
        // corrupt file, or continue running in --paper/--dry-run only.
        if !risk_manager.is_state_verified() {
            return Err(
                "restart-safe risk state is unverifiable (corrupt or unreadable risk_state.json) \
                 — refusing to arm LIVE trading. Restart with --paper or --dry-run only until \
                 the state file is inspected/cleared."
                    .to_string()
                    .into(),
            );
        }
        // Manual arm switch: --confirm-live was already required by
        // validate_signing_config above, but the live loop itself must not
        // start unless the switch is explicitly armed in the risk manager.
        risk_manager.arm_live("--confirm-live provided and validated at startup");
    }

    let metrics_registry = metrics::Metrics::new();
    if let Some(addr) = &args.metrics_addr {
        // Bind synchronously so a bad address fails startup instead of
        // silently running without monitoring.
        let listener = std::net::TcpListener::bind(addr)
            .map_err(|e| format!("metrics server bind failed on {addr}: {e}"))?;
        drop(listener);
        let addr = addr.clone();
        let m = metrics_registry.clone();
        tokio::spawn(async move {
            if let Err(e) = metrics::spawn_metrics_server(&addr, m).await {
                tracing::error!(target: "metrics", error = %e, "metrics server stopped");
            }
        });
    }

    // Graceful shutdown: SIGTERM (systemd stop) or Ctrl-C finishes the
    // current iteration and exits cleanly. An open position stays persisted
    // in live_position.json and is resumed on the next start.
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = shutdown.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = term.recv() => {},
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            tracing::warn!(target: "main", "shutdown signal received — stopping after this iteration");
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    // AŞAMA 2 — paper mode now trades on REAL on-chain market data, which
    // requires the same pool/mint arguments as live. Fail-closed: without
    // them paper refuses to start (no synthetic paper pools anymore).
    if args.paper {
        validate_paper_args(
            args.pool_id.as_deref(),
            args.input_mint.as_deref(),
            args.output_mint.as_deref(),
        )?;
    }

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        args.rpc.clone(),
        CommitmentConfig::confirmed(),
    ));
    let blockhash_mgr = Arc::new(std::sync::Mutex::new(retry::BlockhashManager::new(
        rpc_client.clone(),
    )));

    // Fail-closed signing backend selection. LIVE mode requires the remote HSM
    // (mTLS); the local keyfile is never a fallback for live trading.
    validate_signing_config(
        args.live,
        args.dry_run,
        args.paper,
        args.confirm_live,
        &args.hsm_endpoint,
        &args.hsm_ca,
        &args.hsm_client_identity,
    )?;

    let expected_signer = validate_live_args(
        args.live,
        args.pool_id.as_deref(),
        args.input_mint.as_deref(),
        args.output_mint.as_deref(),
        args.live_risk_data,
        args.expected_signer.as_deref(),
    )?;

    let simulate_payer: Option<Pubkey> = match &args.simulate_payer {
        Some(_) if !args.dry_run => {
            return Err("--simulate-payer is only allowed with --dry-run".into());
        }
        Some(pk) => {
            Some(Pubkey::from_str(pk).map_err(|e| format!("invalid --simulate-payer: {e}"))?)
        }
        None => None,
    };

    let hsm_configured = args.hsm_endpoint.is_some();

    // Restart-safe open position (LIVE only). The record must agree with the
    // risk manager's open-position counter, otherwise the bot could hold a
    // position it cannot exit (fail-closed refuse) or forget one it holds.
    let mut live_position: Option<position::LivePosition> = None;
    if args.live {
        reconcile_pending_entry(&args.data_dir, &rpc_client, &risk_manager)?;
        live_position = position::LivePosition::load(&args.data_dir)
            .map_err(|e| format!("live position record unreadable — refusing to arm LIVE: {e}"))?;
        match (&live_position, risk_manager.open_position_count()) {
            (Some(p), _) => {
                if !p.matches(
                    args.pool_id.as_deref().unwrap_or(""),
                    args.input_mint.as_deref().unwrap_or(""),
                    args.output_mint.as_deref().unwrap_or(""),
                ) {
                    return Err(format!(
                        "an open live position exists for pool {} ({} -> {}); restart with that \
                         --pool-id/--input-mint/--output-mint so it can be exited (fail-closed)",
                        p.pool_id, p.input_mint, p.output_mint
                    )
                    .into());
                }
                if risk_manager.open_position_count() == 0 {
                    // Crash between persisting the record and the counter.
                    risk_manager.record_position_open(p.position_size_lamports);
                }
                tracing::warn!(target: "main", pool_id = %p.pool_id, "resuming management of open live position");
            }
            (None, n) if n > 0 => {
                return Err(format!(
                    "risk state reports {n} open position(s) but no {}/live_position.json record \
                     exists — the position cannot be exited automatically. Close it manually and \
                     clear risk_state.json before arming LIVE (fail-closed).",
                    args.data_dir.display()
                )
                .into());
            }
            _ => {}
        }
        if live_position.is_none() {
            let endpoint = args.hsm_endpoint.as_deref().expect("validated in live");
            let ca = args.hsm_ca.as_deref().expect("validated in live");
            let identity = args
                .hsm_client_identity
                .as_deref()
                .expect("validated in live");
            let signer = hsm_pubkey(endpoint, ca, identity).await?;
            remote_hsm::validate_hsm_pubkey(&signer, expected_signer.as_ref())?;
            let mint = Pubkey::from_str(args.output_mint.as_deref().expect("validated in live"))?;
            let ata = amm::account_resolver::resolve_user_ata(&signer, &mint);
            let account = rpc_client
                .get_account_with_commitment(&ata, CommitmentConfig::confirmed())
                .map_err(|e| format!("startup token reconciliation failed (fail-closed): {e}"))?
                .value;
            if let Some(account) = account {
                if account.owner != spl_token::ID || account.data.len() < 72 {
                    return Err(
                        "startup token reconciliation found an invalid token account (fail-closed)"
                            .into(),
                    );
                }
                let raw_amount =
                    u64::from_le_bytes(account.data[64..72].try_into().expect("8-byte slice"));
                if raw_amount > 0 {
                    return Err(format!(
                        "orphan token balance of {raw_amount} raw units found in {ata}; \
                         refusing live startup without automatic recovery sell (fail-closed)"
                    )
                    .into());
                }
            }
        }
    }

    // The local keyfile is used ONLY for dry-run without a remote HSM. In live
    // mode the remote HSM is mandatory, so the local keyfile is never loaded.
    // In paper mode no signer is loaded at all (no on-chain transaction).
    let local_signer = if args.dry_run && !hsm_configured && simulate_payer.is_none() {
        let kp = load_keypair(&args.wallet)?;
        tracing::info!(target: "main", pubkey = %kp.pubkey(), "local keypair loaded (dry-run only)");
        Some(kp)
    } else {
        None
    };

    let mut total_trades: u64 = 0;
    let mut successful_trades: u64 = 0;
    let mut total_latency_ms: u128 = 0;

    // Operating-mode label for mode-labeled metrics (AŞAMA 3/4): paper,
    // dry_run, live, simulation. Summary/decision records stay separate per
    // mode so simulated results are never mixed with real on-chain ones.
    let mode_label: &str = if args.live {
        "live"
    } else if args.paper {
        "paper"
    } else if args.dry_run {
        "dry_run"
    } else {
        "simulation"
    };

    // Per-RejectReason rejection tally for the paper summary (AŞAMA 3).
    let mut rejected_by_reason: std::collections::HashMap<&'static str, u64> =
        std::collections::HashMap::new();
    // Paper-only accumulators (AŞAMA 3/4): real RPC-fetch and strategy timings
    // plus data-error counts. A paper iteration with a data error is neither a
    // trade nor a success — it is rejected with its real reason.
    let mut paper_sim = PaperSimulator::with_config(strategy_config(&args));
    paper_sim.max_hold_ms = args.max_hold_secs.saturating_mul(1000);
    paper_sim.cost_model = sim_quote::PaperCostModel {
        fee_rate_ppm: 10_000, // replaced by the pool's real fee each tick
        exit_impact_pct: args.exit_impact_pct,
        latency_bps_per_leg: args.paper_latency_bps,
        network_cost_per_tx_lamports: sim_quote::tx_network_cost_lamports(
            args.compute_unit_limit,
            args.priority_fee_micro_lamports,
        ),
    };
    let mut paper_iterations: u64 = 0;
    let mut paper_data_errors: u64 = 0;
    let mut paper_data_fetch_ms: u128 = 0;
    let mut paper_strategy_ms: u128 = 0;
    let mut paper_rejects: u64 = 0;

    // Optional local blocklist, loaded once. Missing file => empty set (not
    // a fail-closed condition — it just means this extra gate is inactive).
    let blocklist: std::collections::HashSet<Pubkey> = match &args.blocklist_file {
        Some(path) => onchain_risk::load_blocklist(path)?,
        None => std::collections::HashSet::new(),
    };

    // Optional real-time WebSocket feed. Starts a background reconnect loop
    // via `MarketDataHandler::start_stream`; the existing RPC-polled
    // `PoolPriceFeed` remains the fail-closed fallback whenever the WS feed
    // has no fresher data yet.
    let ws_provider: Option<Arc<hft_marketdata::solana_ws::SolanaWsProvider>> = if let Some(
        ws_url,
    ) =
        &args.ws_endpoint
    {
        let provider = Arc::new(hft_marketdata::solana_ws::SolanaWsProvider::new(
            ws_url, &args.rpc,
        ));
        hft_marketdata::MarketDataHandler::start_stream(provider.as_ref())
            .map_err(|e| format!("failed to start WebSocket market data stream: {e}"))?;
        tracing::info!(target: "main", ws_endpoint = %ws_url, "WebSocket market data stream started");
        Some(provider)
    } else {
        None
    };

    // Pause between iterations (and after every rejected/failed one). Each
    // iteration makes several RPC calls, so raise this on rate-limited RPC
    // plans (public mainnet RPC answers 429 at the 200 ms default).
    let loop_interval = Duration::from_millis(args.loop_interval_ms.max(50));
    // Jito: a bundle only lands with a tip transfer to one of the Block
    // Engine's tip accounts inside the transaction itself (signed with it).
    // Read the accounts live at startup; refuse to run LIVE through Jito
    // without a valid tip (the bundle would silently never land).
    let jito_tip: Option<(Pubkey, u64)> = match &args.jito_endpoint {
        Some(ep) if (args.live || args.dry_run) && !args.jito_dry_run => {
            if args.jito_tip_lamports < jito::MIN_TIP_LAMPORTS {
                return Err(format!(
                    "--jito-endpoint requires --jito-tip-lamports >= {} (bundles without a tip never land)",
                    jito::MIN_TIP_LAMPORTS
                )
                .into());
            }
            match jito::JitoClient::new(ep, false).tip_accounts().await {
                Ok(accounts) => {
                    let pick = accounts[(marketdata::now_ms() as usize) % accounts.len()];
                    tracing::info!(target: "jito", tip_account = %pick, tip_lamports = args.jito_tip_lamports, "jito tip account resolved");
                    Some((pick, args.jito_tip_lamports))
                }
                Err(e) if args.live => {
                    return Err(
                        format!("cannot resolve Jito tip accounts (fail-closed): {e}").into(),
                    )
                }
                Err(e) => {
                    tracing::warn!(target: "jito", error = %e, "jito tip accounts unavailable — dry-run builds without tip");
                    None
                }
            }
        }
        _ => None,
    };

    // Consecutive failed exit attempts for the open position (widens the
    // exit slippage, see exit_slippage_bps).
    let mut exit_failures: u32 = 0;
    let max_iterations: u64 = if args.iterations == 0 {
        u64::MAX
    } else {
        args.iterations as u64
    };
    let mut completed_iterations: u64 = 0;
    // /health turns 503 when the loop stops iterating. Generous limit: one
    // iteration may legitimately wait up to 180 s for a confirmation.
    metrics_registry.set_health_max_age(240_000 + 10 * args.loop_interval_ms.max(50));
    // Genuine, timestamped pool-price observations for the signal gate.
    let signal_cfg = signal::SignalConfig {
        min_confidence: args.signal_min_confidence.min(100),
        max_sample_age_ms: args.signal_max_age_ms,
        ..signal::SignalConfig::default()
    };
    let mut price_series = signal::Series::new(240, args.signal_sample_ms);
    // Last advisory DexScreener fetch (rate-limited, see the live path).
    let mut dexscreener_last: Option<std::time::Instant> = None;
    // --single-trade / --entry-window-secs (pool discovery supervisor).
    let run_start = std::time::Instant::now();
    let mut single_trade_done = false;
    'main_loop: for i in 0..max_iterations {
        metrics_registry.heartbeat();
        if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if args.single_trade && single_trade_done {
            tracing::info!(target: "main", "single trade completed — stopping");
            break;
        }
        if entry_window_expired(
            run_start.elapsed().as_secs(),
            args.entry_window_secs,
            live_position.is_some() || paper_sim.has_open_position(),
        ) {
            tracing::info!(target: "main", window_secs = args.entry_window_secs, "entry window elapsed without an entry — stopping");
            break;
        }
        completed_iterations = i + 1;
        let iteration_start = std::time::Instant::now();

        metrics::set_risk_gauges(
            &metrics_registry,
            risk_manager.is_circuit_breaker_active(),
            -(risk_manager.current_daily_loss() as i64),
            risk_manager.open_position_count(),
        );
        metrics::set_kill_switch(&metrics_registry, risk_manager.is_kill_switch_active());
        let breaker_state = if risk_manager.is_circuit_breaker_active() {
            2
        } else {
            0
        };
        metrics::set_readiness(
            &metrics_registry,
            metrics::ReadinessSnapshot {
                process_alive: true,
                state_verified: risk_manager.is_state_verified(),
                reconcile_required: false,
                // Exits are retried every iteration and never gated by the
                // entry limits; after this many consecutive failures the
                // operator must look (alert + /ready 503).
                exit_blocked: exit_blocked(live_position.is_some(), exit_failures),
                consecutive_exit_failures: exit_failures,
                breaker_state,
                kill_switch_active: risk_manager.is_kill_switch_active(),
                live_armed: risk_manager.is_live_armed(),
                hsm_ready: args.live.then_some(breaker_state != 2),
                rpc_ready: Some(breaker_state != 2),
                market_data_ready: Some(ws_provider.is_some() || !args.live || breaker_state != 2),
                mode: mode_label.to_string(),
            },
        );

        // LIVE exit management: while a position is open, every iteration
        // evaluates stop-loss / take-profit on the real pool state and sells
        // on a signal. No new entry is attempted while a position is open
        // (max_open_positions = 1), and exits are never blocked by the entry
        // risk gates.
        if args.live {
            if let Some(mut pos) = live_position.clone() {
                let from = match hsm_pubkey(
                    args.hsm_endpoint.as_deref().unwrap_or_default(),
                    args.hsm_ca.as_deref().unwrap_or(Path::new("")),
                    args.hsm_client_identity.as_deref().unwrap_or(Path::new("")),
                )
                .await
                .map_err(|e| format!("{e}"))
                .and_then(|pk| {
                    remote_hsm::validate_hsm_pubkey(&pk, expected_signer.as_ref()).map(|_| pk)
                }) {
                    Ok(pk) => pk,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, "HSM unavailable while a position is open — retrying");
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let exit_start = std::time::Instant::now();
                let outcome = manage_open_position(
                    &args,
                    &rpc_client,
                    &risk_manager,
                    &metrics_registry,
                    &blockhash_mgr,
                    &from,
                    &mut pos,
                    jito_tip,
                    exit_failures,
                )
                .await;
                if matches!(outcome, ExitOutcome::Hold | ExitOutcome::Failed(_)) {
                    // Keep the updated best price for the next iteration.
                    live_position = Some(pos.clone());
                }
                let exit_ms = exit_start.elapsed().as_secs_f64() * 1000.0;
                if !matches!(outcome, ExitOutcome::Hold) {
                    metrics::observe_stage(&metrics_registry, metrics::Stage::Exit, exit_ms);
                }
                exit_failures = match &outcome {
                    ExitOutcome::Failed(_) => exit_failures.saturating_add(1),
                    _ => 0,
                };
                let mut rec = decision::DecisionRecord::new("simple_snipe", "live");
                rec.mode = "live".to_string();
                rec.pool_id = pos.pool_id.clone();
                rec.token_in = pos.output_mint.clone();
                rec.token_out = pos.input_mint.clone();
                match &outcome {
                    ExitOutcome::Hold => {}
                    ExitOutcome::Closed { .. } | ExitOutcome::ClosedExternally => {
                        risk_manager.record_position_close(pos.position_size_lamports);
                        if let Err(e) = position::LivePosition::clear(&args.data_dir) {
                            tracing::error!(target: "live", error = %e, "failed to clear live position record");
                        }
                        live_position = None;
                        single_trade_done = true;
                        if let ExitOutcome::Closed { decision, pnl } = &outcome {
                            successful_trades += 1;
                            metrics::record_trade_executed(&metrics_registry, mode_label);
                            metrics::record_trade(
                                &metrics_registry,
                                true,
                                iteration_start.elapsed().as_millis() as f64,
                            );
                            tracing::info!(target: "live", decision = ?decision, pnl, "position closed — entries re-enabled");
                        } else {
                            tracing::warn!(target: "live", "position tokens no longer in wallet — bookkeeping closed");
                        }
                    }
                    ExitOutcome::Failed(e) => {
                        metrics::record_trade(
                            &metrics_registry,
                            false,
                            iteration_start.elapsed().as_millis() as f64,
                        );
                        tracing::error!(target: "live", error = %e, "exit iteration failed — position stays open, retrying");
                    }
                }
                if !matches!(outcome, ExitOutcome::Hold) {
                    rec.context = serde_json::json!({
                        "action": "exit",
                        "outcome": format!("{outcome:?}"),
                        "latency_ms": { "exit": exit_ms as u64 },
                        "cost_input_units": pos.cost_input_units,
                        "token_amount": pos.token_amount,
                    });
                    rec.save(&args.data_dir)?;
                }
                total_latency_ms =
                    total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());
                sleep(loop_interval).await;
                continue;
            }
        }

        metrics::record_trade_attempt(&metrics_registry, mode_label);
        if let Err(e) = risk_manager.pre_trade_check(sol_to_lamports(0.01), 50) {
            metrics::record_trade_rejected(&metrics_registry, mode_label, e.code());
            *rejected_by_reason.entry(e.code()).or_insert(0) += 1;
            tracing::error!(target: "main", iteration = i, error = %e, "RISK CHECK FAILED — skipping trade");
            if args.live {
                // A tripped circuit breaker is transient (it auto-resets
                // after its cool-down): keep the service alive and retry.
                // Kill switch / daily caps / unverifiable state are not:
                // stop trading.
                if matches!(e, risk::RejectReason::CircuitBreakerOpen) {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                eprintln!("[CRITICAL] Risk check failed: {}. Stopping.", e);
                break;
            }
            sleep(loop_interval).await;
            continue;
        }

        if args.paper {
            // ================================================================
            // PAPER MODE (AŞAMA 2/3/4): runs SimpleSnipeStrategy on REAL
            // on-chain market data (same sources as live), simulates the
            // would-be trade, and NEVER builds, signs or sends a transaction.
            // ================================================================
            paper_iterations += 1;

            // Real market data (pool price, vaults, holders, mint authority).
            let data_start = std::time::Instant::now();
            let data_outcome = resolve_paper_market_data(
                &args,
                &risk_cfg,
                &rpc_client,
                ws_provider.as_deref(),
                &blocklist,
            );
            paper_data_fetch_ms =
                paper_data_fetch_ms.saturating_add(data_start.elapsed().as_millis());
            metrics::observe_stage(
                &metrics_registry,
                metrics::Stage::MarketData,
                data_start.elapsed().as_secs_f64() * 1000.0,
            );

            // A real decision record is written every iteration so audit and
            // replay reflect exactly what happened (including rejections).
            let pool_id_str = args.pool_id.as_deref().unwrap_or("");
            let mut rec = decision::DecisionRecord::new("simple_snipe", "paper");
            rec.mode = "paper".to_string();
            rec.pool_id = pool_id_str.to_string();
            rec.token_in = args.input_mint.as_deref().unwrap_or("").to_string();
            rec.token_out = args.output_mint.as_deref().unwrap_or("").to_string();

            let snapshot = match data_outcome {
                Ok(s) => s,
                Err(PaperDataError::Rejected(reason)) => {
                    // Real reject reason (stale price / token authority risk).
                    let code = reason.code();
                    *rejected_by_reason.entry(code).or_insert(0) += 1;
                    paper_rejects += 1;
                    metrics::record_trade_rejected(&metrics_registry, "paper", code);
                    rec.sqrt_price = String::new();
                    rec.context = serde_json::json!({
                        "action": "rejected",
                        "reject_reason": code,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    tracing::warn!(
                        target: "paper",
                        iteration = i + 1,
                        reject_reason = code,
                        "paper iteration rejected by real risk rule (fail-closed, not simulated-success)"
                    );
                    // Rejection is not a trade: counters stay untouched.
                    total_latency_ms =
                        total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());
                    sleep(loop_interval).await;
                    continue;
                }
                Err(PaperDataError::Unavailable(err)) => {
                    // RPC/parse failure: paper has no breaker and never writes
                    // risk state — counted separately, never simulated-success.
                    paper_data_errors += 1;
                    rec.context = serde_json::json!({
                        "action": "data_error",
                        "error": err,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    tracing::error!(
                        target: "paper",
                        iteration = i + 1,
                        error = %err,
                        "paper market data unavailable — rejected, not simulated-success (fail-closed)"
                    );
                    total_latency_ms =
                        total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());
                    sleep(loop_interval).await;
                    continue;
                }
            };

            rec.sqrt_price = snapshot.current_sqrt_price.to_string();
            rec.liquidity = snapshot.liquidity_lamports.to_string();

            // Real candidate built from on-chain data (same fields the live
            // path feeds into the unchanged SimpleSnipeStrategy::evaluate).
            let candidate = strategy::TokenCandidate {
                liquidity_lamports: snapshot.liquidity_lamports,
                market_cap_lamports: snapshot.market_cap_lamports,
                holders: snapshot.holders,
                is_blocklisted: snapshot.is_blocklisted,
            };

            // Strategy evaluation on the REAL price (unchanged strategy code).
            // Capture the open position's audit fields BEFORE tick: an exit
            // clears the position, so it must be read first to persist the
            // entry/entry-price the P&L was computed from.
            let open_pos = paper_sim.open_position();
            let strat_start = std::time::Instant::now();
            paper_sim.input_is_token0 = snapshot.input_is_token0;
            paper_sim.cost_model.fee_rate_ppm = snapshot.trade_fee_ppm;
            paper_sim.now_ms = marketdata::now_ms() as u64;
            price_series.push(signal::Observation {
                ts_ms: paper_sim.now_ms,
                price: signal::held_token_price(
                    snapshot.current_sqrt_price,
                    snapshot.input_is_token0,
                ),
                liquidity: snapshot.liquidity_lamports,
            });
            let tick = paper_sim.tick(&candidate, snapshot.current_sqrt_price);
            paper_strategy_ms = paper_strategy_ms.saturating_add(strat_start.elapsed().as_millis());

            match tick {
                PaperTick::Hold => {
                    rec.amount_in = 0;
                    rec.context = serde_json::json!({
                        "action": "hold",
                        "entry_sqrt": open_pos.map(|(s, _)| s.to_string()),
                        "position_size_lamports": open_pos.map(|(_, sz)| sz),
                        "current_sqrt": snapshot.current_sqrt_price.to_string(),
                        "source": snapshot.source,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    tracing::info!(
                        target: "paper",
                        iteration = i + 1,
                        "paper position HELD (no exit signal, real price)"
                    );
                }
                PaperTick::ClosedStopLoss(pnl)
                | PaperTick::ClosedTakeProfit(pnl)
                | PaperTick::ClosedForced(_, pnl) => {
                    let action = match &tick {
                        PaperTick::ClosedStopLoss(_) => "exit_stop_loss",
                        PaperTick::ClosedTakeProfit(_) => "exit_take_profit",
                        PaperTick::ClosedForced(strategy::ExitDecision::LiquidityDrop, _) => {
                            "exit_liquidity_drop"
                        }
                        PaperTick::ClosedForced(strategy::ExitDecision::TrailingStop, _) => {
                            "exit_trailing_stop"
                        }
                        _ => "exit_max_hold",
                    };
                    let close = paper_sim.last_close;
                    rec.context = serde_json::json!({
                        "action": action,
                        "realized_simulated_pnl_lamports": pnl,
                        "gross_pnl_lamports": close.map(|c| c.gross_pnl_lamports),
                        "net_pnl_lamports": close.map(|c| c.net_pnl_lamports),
                        "cost": close.map(|c| c.cost),
                        "hold_ms": close.map(|c| c.hold_ms),
                        "mfe_bps": close.map(|c| c.mfe_bps),
                        "mae_bps": close.map(|c| c.mae_bps),
                        "cost_model": "estimate: fee x2 + constant-product impact (exit x exit_impact_pct) + latency bps x2 + network x2",
                        "exit_liquidity_lamports": snapshot.liquidity_lamports,
                        "entry_sqrt": open_pos.map(|(s, _)| s.to_string()),
                        "position_size_lamports": open_pos.map(|(_, sz)| sz),
                        "current_sqrt": snapshot.current_sqrt_price.to_string(),
                        "source": snapshot.source,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    metrics::set_paper_simulated_pnl(&metrics_registry, paper_sim.net_pnl_lamports);
                    single_trade_done = true;
                    tracing::info!(
                        target: "paper",
                        iteration = i + 1,
                        action = action,
                        gross_pnl_lamports = pnl,
                        net_pnl_lamports = close.map(|c| c.net_pnl_lamports),
                        "paper simulated EXIT on real price (no on-chain tx)"
                    );
                }
                PaperTick::NoEntrySignal => {
                    rec.context = serde_json::json!({
                        "action": "no_entry_signal",
                        "liquidity_lamports": snapshot.liquidity_lamports,
                        "holders": snapshot.holders,
                        "blocklisted": snapshot.is_blocklisted,
                        "source": snapshot.source,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    tracing::info!(
                        target: "paper",
                        iteration = i + 1,
                        "strategy produced NO entry signal on real data — no simulated trade"
                    );
                }
                PaperTick::EntryPending(signal) => {
                    if let Some(code) = signal_gate_rejects(&args, &price_series, &signal_cfg) {
                        paper_rejects += 1;
                        *rejected_by_reason.entry(code).or_insert(0) += 1;
                        metrics::record_trade_rejected(&metrics_registry, "paper", code);
                        rec.context = serde_json::json!({
                            "action": "rejected",
                            "reject_reason": code,
                            "samples": price_series.len(),
                            "source": snapshot.source,
                            "simulated": true,
                        });
                        rec.save(&args.data_dir)?;
                        tracing::info!(target: "paper", iteration = i + 1, reason = code, samples = price_series.len(), "signal gate: NO_TRADE");
                        total_latency_ms =
                            total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());
                        sleep(loop_interval).await;
                        continue;
                    }
                    // Real risk gates BEFORE the entry is simulated (same
                    // order as live): max-spend cap, then pre_trade_check.
                    let max_spend_lamports = sol_to_lamports(args.max_spend_sol);
                    let mut accepted = true;
                    if signal.position_size_lamports > max_spend_lamports {
                        accepted = false;
                        *rejected_by_reason.entry("max_spend_exceeded").or_insert(0) += 1;
                        paper_rejects += 1;
                        metrics::record_trade_rejected(
                            &metrics_registry,
                            "paper",
                            "max_spend_exceeded",
                        );
                        tracing::warn!(
                            target: "paper",
                            iteration = i + 1,
                            position_size_lamports = signal.position_size_lamports,
                            max_spend_lamports = max_spend_lamports,
                            "max spend cap exceeded — no simulated trade (fail-closed)"
                        );
                    } else if let Err(e) = risk_manager
                        .pre_trade_check(signal.position_size_lamports, signal.slippage_bps)
                    {
                        accepted = false;
                        *rejected_by_reason.entry(e.code()).or_insert(0) += 1;
                        paper_rejects += 1;
                        metrics::record_trade_rejected(&metrics_registry, "paper", e.code());
                        tracing::warn!(
                            target: "paper",
                            iteration = i + 1,
                            error = %e,
                            "risk gate rejected paper entry — no simulated trade (fail-closed)"
                        );
                    }
                    if !accepted {
                        let code = rejected_by_reason
                            .iter()
                            .max_by_key(|(_, v)| *v)
                            .map(|(k, _)| *k)
                            .unwrap_or("unknown");
                        rec.amount_in = signal.position_size_lamports;
                        rec.context = serde_json::json!({
                            "action": "rejected",
                            "reject_reason": code,
                            "source": snapshot.source,
                            "simulated": true,
                        });
                        rec.save(&args.data_dir)?;
                        total_latency_ms =
                            total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());
                        sleep(loop_interval).await;
                        continue;
                    }
                    // Entry accepted: the ONLY place total_trades and
                    // successful_trades increment (AŞAMA 3 — real signal +
                    // real gates; no unconditional counting).
                    let entry_sqrt = snapshot.current_sqrt_price;
                    paper_sim.confirm_entry(entry_sqrt, signal.clone());
                    total_trades += 1;
                    successful_trades += 1;
                    metrics::record_trade_executed(&metrics_registry, "paper");
                    rec.amount_in = signal.position_size_lamports;
                    rec.context = serde_json::json!({
                        "action": "enter",
                        "would_execute": true,
                        // Recorded so the score can be checked against
                        // outcomes (sniper_report.py groups by it).
                        "signal_confidence": signal_confidence(&price_series, &signal_cfg),
                        "entry_sqrt": entry_sqrt.to_string(),
                        "position_size_lamports": signal.position_size_lamports,
                        "slippage_bps": signal.slippage_bps,
                        "source": snapshot.source,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    tracing::info!(
                        target: "paper",
                        iteration = i + 1,
                        entry_sqrt = entry_sqrt,
                        size_lamports = signal.position_size_lamports,
                        source = snapshot.source,
                        input_is_token0 = snapshot.input_is_token0,
                        "paper ENTRY simulated on real on-chain data (no on-chain tx — would execute)"
                    );
                }
            }

            total_latency_ms =
                total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());

            // Periodic metrics snapshot (paper included; mode-labeled).
            if total_trades > 0 && total_trades.is_multiple_of(10) {
                let avg_latency = total_latency_ms / paper_iterations.max(1) as u128;
                use std::io::Write;
                let metrics_path = args.data_dir.join("metrics.jsonl");
                let metrics = serde_json::json!({
                    "ts_ms": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                    "mode": "paper",
                    "total_trades": total_trades,
                    "successful": successful_trades,
                    "avg_latency_ms": avg_latency
                });
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&metrics_path)
                {
                    let _ = writeln!(f, "{}", metrics);
                }
            }

            sleep(loop_interval).await;
            continue;
        }

        if args.live || args.dry_run {
            // LIVE and DRY-RUN share one execution path so that dry-run
            // exercises the exact same pool-resolution, risk-gate, strategy
            // and transaction-build logic that live will run — the only
            // difference is that dry-run never submits to the network.
            let is_live = args.live;
            let mode_target = if is_live { "live" } else { "dry_run" };
            tracing::info!(
                mode = mode_target,
                iteration = i + 1,
                "LIVE/DRY-RUN mode iteration"
            );

            if risk_manager.is_circuit_breaker_active() {
                tracing::error!(target: "main", "CIRCUIT BREAKER ACTIVE — no new entries until it resets");
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            // Signer resolution: LIVE is fail-closed and requires the remote
            // HSM (validated above); the local keyfile is never loaded in live
            // mode. DRY-RUN may use the remote HSM when configured, otherwise
            // it falls back to the local keyfile (never sent to the network).
            let endpoint = args.hsm_endpoint.as_ref();
            let ca = args.hsm_ca.as_ref();
            let identity = args.hsm_client_identity.as_ref();
            let from = if let Some(payer) = simulate_payer {
                payer
            } else if let (Some(ep), Some(ca), Some(id)) = (endpoint, ca, identity) {
                match hsm_pubkey(ep, ca, id).await.and_then(|pk| {
                    remote_hsm::validate_hsm_pubkey(&pk, expected_signer.as_ref())
                        .map(|_| pk)
                        .map_err(|e| e.into())
                }) {
                    Ok(pk) => pk,
                    Err(e) => {
                        risk_manager
                            .trip_circuit_breaker(&format!("HSM unavailable (fail-closed): {e}"));
                        tracing::error!(mode = mode_target, error = %e, "HSM pubkey fetch failed — circuit breaker tripped");
                        sleep(loop_interval).await;
                        continue;
                    }
                }
            } else if let Some(ref kp) = local_signer {
                kp.pubkey()
            } else {
                risk_manager.trip_circuit_breaker(
                    "no signer available (fail-closed): live requires HSM, dry-run requires HSM or --wallet",
                );
                tracing::error!(
                    mode = mode_target,
                    "no signer available — circuit breaker tripped"
                );
                sleep(loop_interval).await;
                continue;
            };
            let to = from; // self-transfer fallback when no pool is configured

            // When --pool-id is set, resolve the pool on-chain and feed the
            // real price into the strategy (market data hook). Fail-closed:
            // any resolution error halts the loop.
            let mut swap_adapter: Option<amm::raydium_v4::RaydiumV4ClmmAdapter> = None;
            let mut entry_sqrt = 1u128 << 64; // placeholder when no pool is configured
            let mut live_liquidity: Option<u64> = None;
            let mut live_holder_stats: Option<onchain_risk::HolderStats> = None;
            let mut live_blocklisted = false;
            if let Some(pool_id_str) = &args.pool_id {
                let pool_id =
                    Pubkey::from_str(pool_id_str).map_err(|e| format!("invalid --pool-id: {e}"))?;
                let input_mint = args
                    .input_mint
                    .as_ref()
                    .ok_or("--input-mint is required with --pool-id")?;
                let output_mint = args
                    .output_mint
                    .as_ref()
                    .ok_or("--output-mint is required with --pool-id")?;
                let input_mint = Pubkey::from_str(input_mint)
                    .map_err(|e| format!("invalid --input-mint: {e}"))?;
                let output_mint = Pubkey::from_str(output_mint)
                    .map_err(|e| format!("invalid --output-mint: {e}"))?;

                // Select the CLMM program id by cluster (devnet vs mainnet).
                let program_id = clmm_program_id(&args.rpc);

                // Market data hook: prefer a fresh WebSocket update (lower
                // latency) when the feed is connected and has data for this
                // pool; otherwise fall back to the fail-closed RPC-polled hook.
                let ws_state = ws_provider
                    .as_ref()
                    .filter(|p| p.is_connected())
                    .and_then(|p| p.get_pool_state(pool_id_str));
                let price_timestamp_ms: Option<u128>;
                let (_pool, used_ws) = if let Some(state) = ws_state {
                    entry_sqrt = state.sqrt_price;
                    price_timestamp_ms = Some(state.timestamp_ms);
                    (None, true)
                } else {
                    let feed =
                        marketdata::PoolPriceFeed::new(rpc_client.clone(), pool_id, program_id);
                    let pool = match feed.refresh() {
                        Ok(p) => p,
                        Err(e) => {
                            risk_manager.trip_circuit_breaker(&format!(
                                "pool state resolution failed (fail-closed): {e}"
                            ));
                            tracing::error!(mode = mode_target, error = %e, "failed to resolve pool state — circuit breaker tripped");
                            sleep(loop_interval).await;
                            continue;
                        }
                    };
                    entry_sqrt = pool.sqrt_price_x64;
                    price_timestamp_ms = feed
                        .age_ms()
                        .map(|age| marketdata::now_ms().saturating_sub(age));
                    (Some(pool), false)
                };
                tracing::info!(
                    mode = mode_target,
                    pool_id = %pool_id,
                    sqrt_price = entry_sqrt,
                    source = if used_ws { "websocket" } else { "rpc_poll" },
                    "resolved pool state — feeding real price into strategy"
                );

                // Fail-closed price staleness gate: missing or stale
                // timestamps reject the trade rather than proceeding with
                // unknown-freshness data.
                if let Some(ts) = price_timestamp_ms {
                    if let Err(reason) =
                        risk::check_price_staleness(ts, risk_cfg.price_staleness_ms)
                    {
                        tracing::warn!(mode = mode_target, iteration = i + 1, reason = %reason, "price staleness check failed — no trade this iteration (fail-closed)");
                        sleep(loop_interval).await;
                        continue;
                    }
                } else {
                    tracing::warn!(
                        mode = mode_target,
                        iteration = i + 1,
                        "no price timestamp available — rejecting trade (fail-closed)"
                    );
                    sleep(loop_interval).await;
                    continue;
                }

                // Resolve the full swap account set deterministically.
                let (accounts, resolved) = match amm::account_resolver::resolve_swap_accounts(
                    &rpc_client,
                    &pool_id,
                    &from,
                    &input_mint,
                    &output_mint,
                    &program_id,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        risk_manager.trip_circuit_breaker(&format!(
                            "swap account resolution failed (fail-closed): {e}"
                        ));
                        tracing::error!(mode = mode_target, error = %e, "failed to resolve swap accounts — circuit breaker tripped");
                        sleep(loop_interval).await;
                        continue;
                    }
                };

                // Mint/freeze authority rug-check: always enforced when a
                // pool is configured, independent of --live-risk-data. A
                // present mint or freeze authority means the token issuer
                // can mint more supply or freeze accounts at will — reject
                // fail-closed.
                for (label, mint) in [("input", input_mint), ("output", output_mint)] {
                    match onchain_risk::fetch_mint_authority_risk_cached(&rpc_client, &mint) {
                        Ok(risk) if risk.is_risky() => {
                            tracing::warn!(
                                mode = mode_target,
                                iteration = i + 1,
                                mint_role = label,
                                mint = %mint,
                                mint_authority_present = risk.mint_authority_present,
                                freeze_authority_present = risk.freeze_authority_present,
                                "token authority risk detected — no trade this iteration (fail-closed)"
                            );
                            sleep(loop_interval).await;
                            continue 'main_loop;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            risk_manager.trip_circuit_breaker(&format!(
                                "mint authority check failed (fail-closed): {e}"
                            ));
                            tracing::error!(mode = mode_target, error = %e, mint_role = label, "mint authority check failed — circuit breaker tripped");
                            sleep(loop_interval).await;
                            continue 'main_loop;
                        }
                    }
                }

                if args.live_risk_data {
                    // Fail-closed: any RPC error here trips the circuit
                    // breaker and skips this iteration rather than crashing
                    // the whole process. This replaces the static
                    // --pool-liquidity / --pool-holders CLI values with real
                    // on-chain data.
                    let liquidity = match onchain_risk::fetch_vault_liquidity(
                        &rpc_client,
                        &accounts.input_vault,
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            risk_manager.trip_circuit_breaker(&format!(
                                "live risk data (fail-closed): {e}"
                            ));
                            tracing::error!(mode = mode_target, error = %e, "liquidity fetch failed — circuit breaker tripped");
                            sleep(loop_interval).await;
                            continue;
                        }
                    };
                    // Holder risk of the token being bought (never wSOL).
                    let holder_stats = match onchain_risk::fetch_holder_stats_cached(
                        &rpc_client,
                        &output_mint,
                        &[accounts.input_vault, accounts.output_vault],
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            risk_manager.trip_circuit_breaker(&format!(
                                "live risk data (fail-closed): {e}"
                            ));
                            tracing::error!(mode = mode_target, error = %e, "holder stats fetch failed — circuit breaker tripped");
                            sleep(loop_interval).await;
                            continue;
                        }
                    };
                    // Holder-concentration gate (operator-approved design):
                    // same 30/70 thresholds as the paper path. A breach is an
                    // explicit rejection, never a silent no_entry_signal.
                    match onchain_risk::holder_concentration_verdict(&holder_stats) {
                        onchain_risk::HolderVerdict::Ok => {}
                        verdict => {
                            tracing::warn!(
                                mode = mode_target,
                                verdict = ?verdict,
                                "holder concentration gate rejected candidate (fail-closed)"
                            );
                            metrics::record_trade_rejected(
                                &metrics_registry,
                                mode_label,
                                risk::RejectReason::HolderConcentrationExceeded.code(),
                            );
                            sleep(loop_interval).await;
                            continue;
                        }
                    }
                    tracing::info!(
                        mode = mode_target,
                        liquidity,
                        top_holder_pct = holder_stats.top_holder_pct,
                        top20_holder_pct = holder_stats.top20_holder_pct,
                        program_owned_pct = holder_stats.program_owned_pct,
                        sampled_holders = holder_stats.sampled_holders,
                        "fetched real on-chain liquidity + holder concentration"
                    );
                    live_blocklisted =
                        blocklist.contains(&input_mint) || blocklist.contains(&output_mint);
                    live_liquidity = Some(liquidity);
                    live_holder_stats = Some(holder_stats);
                }

                // Advisory only — never gates the trade. A blocking HTTP call,
                // so at most once a minute and off the async worker.
                if args.dexscreener_check
                    && dexscreener_last
                        .is_none_or(|t: std::time::Instant| t.elapsed() >= Duration::from_secs(60))
                {
                    dexscreener_last = Some(std::time::Instant::now());
                    match tokio::task::block_in_place(|| discovery::fetch_snapshot(pool_id_str)) {
                        Ok(snapshot) => tracing::info!(
                            mode = mode_target,
                            liquidity_usd = snapshot.liquidity_usd,
                            fdv = snapshot.fdv,
                            volume_24h_usd = snapshot.volume_24h_usd,
                            "dexscreener advisory snapshot (not used for trading gate)"
                        ),
                        Err(e) => tracing::warn!(
                            mode = mode_target,
                            error = %e,
                            "dexscreener advisory check failed — ignored, not gating trade"
                        ),
                    }
                }

                let adapter = amm::raydium_v4::RaydiumV4ClmmAdapter::new(pool_id_str.clone())
                    .with_program_id(program_id.to_string())
                    .with_swap_accounts(accounts)
                    .with_resolved_pool(resolved);
                price_series.push(signal::Observation {
                    ts_ms: marketdata::now_ms() as u64,
                    price: signal::held_token_price(entry_sqrt, adapter.input_is_token0()),
                    liquidity: live_liquidity.unwrap_or(0),
                });
                swap_adapter = Some(adapter);
            }

            // Strategy gate: evaluate the token candidate. If the strategy
            // rejects it (fail-closed), no trade is built or sent this
            // iteration. This wires SimpleSnipeStrategy into the live path.
            let strategy = strategy::SimpleSnipeStrategy::new(strategy_config(&args));
            let candidate = strategy::TokenCandidate {
                liquidity_lamports: live_liquidity.unwrap_or(args.pool_liquidity),
                market_cap_lamports: args.pool_market_cap,
                holders: live_holder_stats
                    .as_ref()
                    .map(|h| h.sampled_holders)
                    .unwrap_or(args.pool_holders),
                is_blocklisted: args.pool_blocklisted
                    || live_blocklisted
                    || live_holder_stats
                        .as_ref()
                        .is_some_and(|h| h.top_holder_pct > args.max_top_holder_pct),
            };
            let entry_signal = strategy.evaluate(&candidate, entry_sqrt);

            let mut rec = decision::DecisionRecord::new("simple_snipe", mode_target);
            rec.mode = mode_target.to_string();
            rec.pool_id = args.pool_id.clone().unwrap_or_default();
            rec.token_in = args.input_mint.clone().unwrap_or_default();
            rec.token_out = args.output_mint.clone().unwrap_or_default();
            rec.liquidity = candidate.liquidity_lamports.to_string();
            rec.context = serde_json::json!({
                "entry": entry_signal.is_some(),
                "market_cap": candidate.market_cap_lamports,
                "holders": candidate.holders,
                "blocklisted": candidate.is_blocklisted,
            });

            // Offline HSM smoke mode: `--dry-run` WITHOUT `--pool-id` — this is
            // exactly what the CI mTLS smoke test invokes (fixed --blockhash,
            // no RPC) to verify the HSM signing + audit pipeline. There is no
            // market data for the strategy to evaluate (the static candidate
            // args default to zero), so the gate would reject every iteration
            // and nothing would ever reach the signer. Preserve the legacy
            // dry-run contract for that invocation: fall through with the
            // default-size signal so the fallback self-transfer tx is still
            // built and signed (never sent). Never applies to live, and never
            // applies when --pool-id IS set — the AŞAMA 1 rehearsal always
            // passes --pool-id, so it goes through the real gates above.
            let entry_signal = match entry_signal {
                Some(sig) => sig,
                None if args.pool_id.is_none() && !is_live => {
                    tracing::info!(
                        mode = mode_target,
                        iteration = i + 1,
                        "offline HSM smoke mode (no --pool-id): bypassing market-data strategy gate and building the fallback signed tx"
                    );
                    rec.context = serde_json::json!({
                        "entry": true,
                        "offline_hsm_smoke": true,
                    });
                    strategy::EntrySignal {
                        position_size_lamports: strategy.config().max_trade_size_lamports,
                        slippage_bps: strategy.config().max_slippage_bps,
                        entry_sqrt_price: entry_sqrt,
                    }
                }
                None => {
                    tracing::warn!(
                        mode = mode_target,
                        iteration = i + 1,
                        liquidity = candidate.liquidity_lamports,
                        market_cap = candidate.market_cap_lamports,
                        holders = candidate.holders,
                        blocklisted = candidate.is_blocklisted,
                        "strategy rejected candidate — no trade this iteration (fail-closed)"
                    );
                    rec.save(&args.data_dir)?;
                    total_trades += 1;
                    sleep(loop_interval).await;
                    continue;
                }
            };
            if args.pool_id.is_some() {
                if let Some(code) = signal_gate_rejects(&args, &price_series, &signal_cfg) {
                    metrics::record_trade_rejected(&metrics_registry, mode_label, code);
                    tracing::info!(
                        mode = mode_target,
                        iteration = i + 1,
                        reason = code,
                        samples = price_series.len(),
                        "signal gate: NO_TRADE"
                    );
                    rec.context = serde_json::json!({
                        "action": "rejected",
                        "reject_reason": code,
                        "samples": price_series.len(),
                    });
                    rec.save(&args.data_dir)?;
                    sleep(loop_interval).await;
                    continue;
                }
            }
            rec.amount_in = entry_signal.position_size_lamports;
            rec.save(&args.data_dir)?;

            // Security: max spend (SOL) cap enforced before any trade.
            let max_spend_lamports = sol_to_lamports(args.max_spend_sol);
            if entry_signal.position_size_lamports > max_spend_lamports {
                tracing::warn!(
                    mode = mode_target,
                    iteration = i + 1,
                    position_size_lamports = entry_signal.position_size_lamports,
                    max_spend_lamports = max_spend_lamports,
                    "max spend (SOL) cap exceeded — no trade this iteration (fail-closed)"
                );
                metrics::record_trade_rejected(&metrics_registry, mode_label, "max_spend_exceeded");
                rec.context = serde_json::json!({
                    "entry": true,
                    "risk_rejected": "max_spend_sol_exceeded",
                });
                rec.save(&args.data_dir)?;
                total_trades += 1;
                sleep(loop_interval).await;
                continue;
            }

            // Risk gate: enforce kill switch, daily trade cap, position size,
            // and slippage before building any transaction. Fail-closed: if
            // any limit is exceeded, no trade is built or sent this iteration.
            if let Err(e) = risk_manager.pre_trade_check(
                entry_signal.position_size_lamports,
                entry_signal.slippage_bps,
            ) {
                metrics::record_trade_rejected(&metrics_registry, mode_label, e.code());
                tracing::warn!(
                    mode = mode_target,
                    iteration = i + 1,
                    error = %e,
                    "risk gate rejected trade — no trade this iteration (fail-closed)"
                );
                rec.context = serde_json::json!({
                    "entry": true,
                    "risk_rejected": e,
                });
                rec.save(&args.data_dir)?;
                total_trades += 1;
                sleep(loop_interval).await;
                continue;
            }

            // Fresh blockhash per iteration to avoid replay (dry-run may pin
            // one with --blockhash for offline/deterministic smoke tests).
            let blockhash = match &args.blockhash {
                Some(_) if !is_live => resolve_blockhash(&args, &blockhash_mgr),
                _ => blockhash_mgr.lock().unwrap().force_refresh(),
            };
            let blockhash = match blockhash {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(mode = mode_target, error = %e, "blockhash unavailable — no trade this iteration");
                    sleep(loop_interval).await;
                    continue;
                }
            };
            {
                // Build the transaction: a real AMM swap when a pool is
                // configured, otherwise (dry-run only) the self-transfer
                // HSM smoke path.
                let mut swap_min_output: u64 = 0;
                let mut tx = if let Some(adapter) = &swap_adapter {
                    // Real swap: quote from the resolved on-chain price, apply
                    // slippage (min_amount_out), and build the full CLMM swap
                    // tx (compute budget, ATAs, SOL wrap/unwrap).
                    let opts = amm::raydium_v4::SwapTxOptions {
                        input_mint: Pubkey::from_str(
                            args.input_mint.as_deref().unwrap_or_default(),
                        )
                        .map_err(|e| format!("invalid --input-mint: {e}"))?,
                        output_mint: Pubkey::from_str(
                            args.output_mint.as_deref().unwrap_or_default(),
                        )
                        .map_err(|e| format!("invalid --output-mint: {e}"))?,
                        compute_unit_limit: args.compute_unit_limit,
                        priority_fee_micro_lamports: args.priority_fee_micro_lamports,
                        close_input_token_account: false,
                        jito_tip,
                        keep_output_wrapped: false,
                    };
                    // Exact quote from an on-chain simulation of this very
                    // swap (price impact and tick crossings included), then
                    // the economics gate. Any failure: no trade.
                    let quote_start = std::time::Instant::now();
                    let quoted = exact_quote(
                        &rpc_client,
                        adapter,
                        entry_signal.position_size_lamports,
                        args.max_slippage_bps,
                        &from,
                        &opts,
                    );
                    let quote_ms = quote_start.elapsed().as_secs_f64() * 1000.0;
                    metrics::observe_stage(&metrics_registry, metrics::Stage::QuoteSim, quote_ms);
                    let (spot, sim) = match quoted {
                        Ok(v) => v,
                        Err(e) => {
                            metrics::record_trade_rejected(
                                &metrics_registry,
                                mode_label,
                                "quote_simulation_failed",
                            );
                            tracing::warn!(mode = mode_target, error = %e, "exact quote unavailable — no trade (fail-closed)");
                            rec.context = serde_json::json!({
                                "action": "rejected",
                                "reject_reason": "quote_simulation_failed",
                                "error": e,
                            });
                            rec.save(&args.data_dir)?;
                            sleep(loop_interval).await;
                            continue;
                        }
                    };
                    let economics = entry_economics(
                        &args,
                        spot.expected_output,
                        sim.amount_out,
                        adapter.trade_fee_ppm(),
                        entry_signal.position_size_lamports,
                        jito_tip.map(|(_, l)| l).unwrap_or(0),
                        live_liquidity,
                    );
                    let edge = match economics {
                        Ok(v) => v,
                        Err(code) => {
                            metrics::record_trade_rejected(&metrics_registry, mode_label, code);
                            tracing::warn!(
                                mode = mode_target,
                                reason = code,
                                spot_out = spot.expected_output,
                                exact_out = sim.amount_out,
                                "entry economics rejected — no trade"
                            );
                            rec.context = serde_json::json!({
                                "action": "rejected",
                                "reject_reason": code,
                                "spot_out": spot.expected_output,
                                "exact_out": sim.amount_out,
                                "liquidity_lamports": live_liquidity,
                            });
                            rec.save(&args.data_dir)?;
                            sleep(loop_interval).await;
                            continue;
                        }
                    };
                    tracing::info!(
                        mode = mode_target,
                        spot_out = spot.expected_output,
                        exact_out = sim.amount_out,
                        impact_bps = edge.entry_impact_bps,
                        exit_impact_est_bps = edge.exit_impact_bps,
                        exit_cost_bps = edge.exit_cost_bps,
                        expected_return_bps = edge.expected_return_bps,
                        edge_bps = edge.net_edge_bps,
                        worst_case_edge_bps = edge.worst_case_edge_bps,
                        compute_units = ?sim.compute_units,
                        "exact entry quote (on-chain simulation)"
                    );
                    rec.context = serde_json::json!({
                        "action": "quoted",
                        "signal_confidence": signal_confidence(&price_series, &signal_cfg),
                        "latency_ms": { "quote_sim": quote_ms as u64 },
                        "spot_out": spot.expected_output,
                        "exact_out": sim.amount_out,
                        "liquidity_lamports": live_liquidity,
                        "edge": edge,
                    });
                    rec.save(&args.data_dir)?;
                    let mut exact = spot;
                    exact.expected_output = sim.amount_out;
                    let intent = adapter
                        .build_intent(exact)
                        .map_err(|e| format!("build_intent failed (fail-closed): {e}"))?;
                    swap_min_output = intent.min_output;
                    adapter
                        .build_swap_transaction(&intent, &from, blockhash, &opts)
                        .map_err(|e| format!("build_transaction failed (fail-closed): {e}"))?
                } else {
                    // Dynamic compute units per iteration for uniqueness
                    let cu_limit = 400 + (i % 200) as u32;
                    let cu_ix =
                        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
                            cu_limit,
                        );
                    let transfer_ix =
                        solana_system_interface::instruction::transfer(&from, &to, 1_000);
                    let msg = solana_sdk::message::Message::new(&[cu_ix, transfer_ix], Some(&from));
                    let mut t = Transaction::new_unsigned(msg);
                    t.message.recent_blockhash = blockhash;
                    t
                };
                // Final pre-send recheck: re-validate kill switch, circuit
                // breaker, daily limits, slippage, max-spend, and position
                // caps immediately before signing — state may have changed
                // since the earlier check at the top of this iteration.
                if let Err(e) = risk_manager.pre_trade_check(
                    entry_signal.position_size_lamports,
                    entry_signal.slippage_bps,
                ) {
                    metrics::record_trade_rejected(&metrics_registry, mode_label, e.code());
                    tracing::warn!(mode = mode_target, iteration = i + 1, error = %e, "final pre-send risk recheck failed — aborting send (fail-closed)");
                    sleep(loop_interval).await;
                    continue;
                }

                // Sign (HSM when configured; dry-run may use the local
                // keypair) and VERIFY the signature against the expected
                // signer before it is attached or sent.
                if simulate_payer.is_some() {
                    // Unsigned simulation build: nothing to sign.
                } else if let Err(e) = sign_and_verify(
                    &args,
                    local_signer.as_ref(),
                    &from,
                    &mut tx,
                    &metrics_registry,
                )
                .await
                {
                    risk_manager
                        .trip_circuit_breaker(&format!("signing failed (fail-closed): {e}"));
                    tracing::error!(mode = mode_target, error = %e, "signing/verification failed — circuit breaker tripped");
                    sleep(loop_interval).await;
                    continue;
                }

                // Submission: LIVE sends the signed transaction and waits for
                // confirmation. DRY-RUN never submits — it prints the built
                // and signed transaction so an operator can inspect exactly
                // what live would have sent.
                if !is_live {
                    let tx_bytes = bincode::serialize(&tx).unwrap_or_default();
                    println!(
                        "[DRY-RUN] iter {}: tx (hex) = {}",
                        i + 1,
                        hex::encode(&tx_bytes)
                    );
                    println!("[DRY-RUN] iter {}: signature = {}", i + 1, tx.signatures[0]);
                    // Real swap => simulate it on-chain (never sent). This is
                    // the end-to-end check of program id, accounts, ATAs,
                    // wSOL wrap, tick arrays and slippage against mainnet.
                    if swap_adapter.is_some() {
                        match simulate_on_chain(&rpc_client, &tx) {
                            Ok(sim) => {
                                let ok = sim.err.is_none();
                                println!(
                                    "[DRY-RUN] iter {}: on-chain simulation {} (compute units {:?}){}",
                                    i + 1,
                                    if ok { "OK" } else { "FAILED" },
                                    sim.units,
                                    sim.err.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                                );
                                for line in sim.logs.iter().rev().take(12).rev() {
                                    println!("    {line}");
                                }
                                tracing::info!(
                                    target: "dry_run",
                                    simulation_ok = ok,
                                    units = sim.units,
                                    error = sim.err.as_deref().unwrap_or(""),
                                    "on-chain simulation of the dry-run swap"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(target: "dry_run", error = %e, "on-chain simulation unavailable")
                            }
                        }
                        // Exit (stop-loss / take-profit sell) path: the same
                        // builder the live exit uses, selling the amount this
                        // entry would buy back into the input mint. The fee
                        // payer must hold that token for the simulation to
                        // pass (an "insufficient funds" result then only means
                        // the payer holds none, not a broken exit).
                        match build_exit_tx(
                            &args,
                            &rpc_client,
                            &from,
                            swap_min_output,
                            blockhash,
                            jito_tip,
                        )
                        .and_then(|exit_tx| simulate_on_chain(&rpc_client, &exit_tx))
                        {
                            Ok(sim) => {
                                let ok = sim.err.is_none();
                                println!(
                                    "[DRY-RUN] iter {}: EXIT (sell {} raw units) on-chain simulation {} (compute units {:?}){}",
                                    i + 1,
                                    swap_min_output,
                                    if ok { "OK" } else { "FAILED" },
                                    sim.units,
                                    sim.err.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                                );
                                for line in sim.logs.iter().rev().take(12).rev() {
                                    println!("    {line}");
                                }
                            }
                            Err(e) => println!(
                                "[DRY-RUN] iter {}: EXIT simulation unavailable: {e}",
                                i + 1
                            ),
                        }
                        // The payer usually holds none of a fresh launch
                        // token, so its exit simulation cannot pass. Run the
                        // same exit once more as an existing wallet holder
                        // of the token (read-only simulation, nothing is
                        // signed): a pass shows the exit path itself works
                        // on this pool.
                        let held =
                            Pubkey::from_str(args.output_mint.as_deref().unwrap_or_default());
                        if let Ok(held) = held {
                            match onchain_risk::find_simulation_holder(
                                &rpc_client,
                                &held,
                                swap_min_output,
                            ) {
                                Ok(Some(holder)) => match build_exit_tx(
                                    &args,
                                    &rpc_client,
                                    &holder,
                                    swap_min_output,
                                    blockhash,
                                    jito_tip,
                                )
                                .and_then(|t| simulate_on_chain(&rpc_client, &t))
                                {
                                    Ok(sim) => {
                                        let ok = sim.err.is_none();
                                        println!(
                                            "[DRY-RUN] iter {}: EXIT simulation as token holder {holder}: {} (compute units {:?}){}",
                                            i + 1,
                                            if ok { "OK" } else { "FAILED" },
                                            sim.units,
                                            sim.err.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                                        );
                                        if !ok {
                                            for line in sim.logs.iter().rev().take(8).rev() {
                                                println!("    {line}");
                                            }
                                        }
                                    }
                                    Err(e) => println!(
                                        "[DRY-RUN] iter {}: EXIT simulation as token holder unavailable: {e}",
                                        i + 1
                                    ),
                                },
                                Ok(None) => println!(
                                    "[DRY-RUN] iter {}: no wallet holds >= {swap_min_output} of the token in its ATA — exit path unverified on this pool",
                                    i + 1
                                ),
                                Err(e) => println!(
                                    "[DRY-RUN] iter {}: token holder lookup failed: {e}",
                                    i + 1
                                ),
                            }
                        }
                    }
                    tracing::info!(
                        target: "dry_run",
                        signature = %tx.signatures[0],
                        "dry-run transaction built and signed (never sent)"
                    );
                    successful_trades += 1;
                    single_trade_done = true;
                    // NOTE: dry-run does NOT call risk_manager.record_trade()
                    // or record_position_open() — those represent real
                    // on-chain position/trade accounting. Dry-run only
                    // builds + signs; it never opens a real position.
                    metrics::record_trade_executed(&metrics_registry, mode_label);
                } else {
                    // Real balances before/after the entry give the true cost
                    // (fees included) and the real token amount received —
                    // the numbers the exit path values and sells.
                    let in_mint = Pubkey::from_str(args.input_mint.as_deref().unwrap_or_default())
                        .map_err(|e| format!("invalid --input-mint: {e}"))?;
                    let out_mint =
                        Pubkey::from_str(args.output_mint.as_deref().unwrap_or_default())
                            .map_err(|e| format!("invalid --output-mint: {e}"))?;
                    let before_in = position::fetch_mint_balance(&rpc_client, &from, &in_mint);
                    let before_out = position::fetch_mint_balance(&rpc_client, &from, &out_mint);
                    let (before_in, before_out) = match (before_in, before_out) {
                        (Ok(a), Ok(b)) => (a, b),
                        (Err(e), _) | (_, Err(e)) => {
                            tracing::error!(mode = mode_target, error = %e, "pre-entry balance fetch failed — no trade (fail-closed)");
                            sleep(loop_interval).await;
                            continue;
                        }
                    };
                    // Funds check BEFORE sending: the trade size plus a SOL
                    // reserve (ATA rent for a new token account, wSOL account,
                    // network + priority fees, and the later exit's fees).
                    // Without it an underfunded wallet would fail every
                    // iteration and each failure was booked as a loss.
                    let required = entry_required_balance(
                        entry_signal.position_size_lamports,
                        in_mint == native_mint(),
                    );
                    if before_in < required {
                        tracing::warn!(
                            mode = mode_target,
                            balance = before_in,
                            required,
                            "insufficient balance for entry + SOL reserve — no trade (fund the wallet)"
                        );
                        metrics::record_trade_rejected(
                            &metrics_registry,
                            mode_label,
                            "insufficient_balance",
                        );
                        sleep(loop_interval).await;
                        continue;
                    }
                    // Crash safety: record the entry BEFORE it is sent. If the
                    // process dies while waiting for the confirmation, the
                    // next start books the position from the chain.
                    let pending = position::PendingEntry {
                        signature: tx.signatures[0].to_string(),
                        blockhash: tx.message.recent_blockhash.to_string(),
                        before_in,
                        before_out,
                        position: position::LivePosition {
                            pool_id: args.pool_id.clone().unwrap_or_default(),
                            input_mint: args.input_mint.clone().unwrap_or_default(),
                            output_mint: args.output_mint.clone().unwrap_or_default(),
                            entry_sqrt_price: entry_sqrt.to_string(),
                            position_size_lamports: entry_signal.position_size_lamports,
                            cost_input_units: 0,
                            token_amount: 0,
                            entry_signature: tx.signatures[0].to_string(),
                            opened_at_ms: marketdata::now_ms() as u64,
                            entry_liquidity_lamports: live_liquidity.unwrap_or(0),
                            peak_value_ratio: 0,
                        },
                    };
                    if let Err(e) = pending.save(&args.data_dir) {
                        tracing::error!(mode = mode_target, error = %e, "cannot persist the pending entry — no trade (fail-closed)");
                        sleep(loop_interval).await;
                        continue;
                    }
                    let submit_start = std::time::Instant::now();
                    let submitted = submit_live_tx(&args, &rpc_client, &tx).await;
                    metrics::observe_stage(
                        &metrics_registry,
                        metrics::Stage::SubmitConfirm,
                        submit_start.elapsed().as_secs_f64() * 1000.0,
                    );
                    // An Err may mean "outcome unknown" (e.g. confirmation
                    // timeout). Reconcile from the real token balance before
                    // concluding nothing happened, so a swap that landed late
                    // is never left unmanaged (and never bought twice).
                    let outcome: Result<Signature, String> = match submitted {
                        Ok(sig) => Ok(sig),
                        Err(e) => {
                            match position::fetch_mint_balance(&rpc_client, &from, &out_mint) {
                                Ok(after) if after > before_out => {
                                    tracing::warn!(mode = mode_target, error = %e, received = after - before_out, "submission reported an error but the swap LANDED — recording the position");
                                    Ok(tx.signatures[0])
                                }
                                Ok(_) => {
                                    // Definitely not landed (the confirmation
                                    // waits for the blockhash to expire).
                                    let _ = position::PendingEntry::clear(&args.data_dir);
                                    Err(format!("{e}"))
                                }
                                Err(be) => {
                                    // pending_entry.json stays: the next
                                    // start reconciles it from the chain.
                                    risk_manager.trigger_kill_switch(&format!(
                                        "entry outcome unknown (send: {e}; balance check: {be}) — inspect the wallet before re-arming"
                                    ));
                                    Err(format!("{e} — outcome unknown, kill switch engaged"))
                                }
                            }
                        }
                    };
                    match outcome {
                        Ok(sig) => {
                            successful_trades += 1;
                            let cost = position::fetch_mint_balance(&rpc_client, &from, &in_mint)
                                .map(|after| before_in.saturating_sub(after))
                                .unwrap_or(entry_signal.position_size_lamports);
                            let tokens =
                                position::fetch_mint_balance(&rpc_client, &from, &out_mint)
                                    .map(|after| after.saturating_sub(before_out))
                                    .unwrap_or(swap_min_output);
                            let pos = position::LivePosition {
                                pool_id: args.pool_id.clone().unwrap_or_default(),
                                input_mint: args.input_mint.clone().unwrap_or_default(),
                                output_mint: args.output_mint.clone().unwrap_or_default(),
                                entry_sqrt_price: entry_sqrt.to_string(),
                                position_size_lamports: entry_signal.position_size_lamports,
                                cost_input_units: cost.max(1),
                                token_amount: tokens,
                                entry_signature: sig.to_string(),
                                opened_at_ms: marketdata::now_ms() as u64,
                                entry_liquidity_lamports: live_liquidity.unwrap_or(0),
                                peak_value_ratio: 0,
                            };
                            // Persist the position BEFORE the counters so a
                            // crash in between is recoverable on restart.
                            if let Err(e) = pos.save(&args.data_dir) {
                                tracing::error!(mode = mode_target, error = %e, "failed to persist live position — halting new entries");
                                risk_manager.trigger_kill_switch(&format!(
                                    "live position persist failed: {e}"
                                ));
                            }
                            risk_manager.record_trade();
                            risk_manager.record_position_open(entry_signal.position_size_lamports);
                            if let Err(e) = position::PendingEntry::clear(&args.data_dir) {
                                tracing::warn!(mode = mode_target, error = %e, "pending entry not cleared (harmless: the position record exists)");
                            }
                            live_position = Some(pos);
                            metrics::record_trade_executed(&metrics_registry, mode_label);
                            metrics::record_trade(
                                &metrics_registry,
                                true,
                                iteration_start.elapsed().as_millis() as f64,
                            );
                            tracing::info!(mode = mode_target, signature = %sig, cost, tokens, "ENTRY confirmed — position open, exit management active");
                            let cluster = if args.rpc.contains("devnet") {
                                "devnet"
                            } else {
                                "mainnet-beta"
                            };
                            println!(
                                "[LIVE] iter {}: ENTRY confirmed: https://explorer.solana.com/tx/{}?cluster={}",
                                i + 1,
                                sig,
                                cluster
                            );
                        }
                        Err(e) => {
                            metrics::record_trade(
                                &metrics_registry,
                                false,
                                iteration_start.elapsed().as_millis() as f64,
                            );
                            // A failed tx still burns its network fee.
                            let _ = risk_manager.record_loss(5_000);
                            tracing::error!(mode = mode_target, error = %e, "transaction failed after retries");
                            eprintln!("[LIVE] iter {}: TX failed: {}", i + 1, e);
                        }
                    }
                } // end else: live-only send path
            }
        } else {
            tracing::debug!(target: "sim", iteration = i + 1, "simulation iteration");
            // Simulation (no flags): synthetic iteration counts as an
            // executed trade in the mode-labeled metrics — matching the
            // legacy unconditional `total_trades += 1` semantics below.
            metrics::record_trade_executed(&metrics_registry, mode_label);
        }

        let elapsed_ms = iteration_start.elapsed().as_millis();
        total_latency_ms = total_latency_ms.saturating_add(elapsed_ms);
        total_trades += 1;

        if total_trades.is_multiple_of(10) {
            let avg_latency = if total_trades > 0 {
                total_latency_ms / total_trades as u128
            } else {
                0
            };
            tracing::info!(
                target: "metrics",
                total_trades = total_trades,
                successful = successful_trades,
                avg_latency_ms = avg_latency,
                "metrics snapshot"
            );

            use std::io::Write;
            let metrics_path = args.data_dir.join("metrics.jsonl");
            let metrics = serde_json::json!({
                "ts_ms": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                "total_trades": total_trades,
                "successful": successful_trades,
                "avg_latency_ms": avg_latency
            });
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&metrics_path)
            {
                let _ = writeln!(f, "{}", metrics);
            }
        }

        sleep(loop_interval).await;
    }

    // Paper iterations all accumulate latency (rejections included), while
    // `total_trades` counts only accepted simulated entries — so for paper
    // the per-iteration average is the honest denominator.
    let avg_latency_denom = if args.paper {
        paper_iterations
    } else {
        total_trades
    };
    let avg_latency = if avg_latency_denom > 0 {
        total_latency_ms / avg_latency_denom as u128
    } else {
        0
    };
    tracing::info!(
        target: "main",
        total_trades = total_trades,
        successful = successful_trades,
        avg_latency_ms = avg_latency,
        "Solana HFT platform finished"
    );

    println!();
    println!("========================================");
    println!("  HFT Platform Summary");
    println!("========================================");
    println!("  Total iterations: {}", completed_iterations);
    println!("  Total trades:     {}", total_trades);
    println!("  Successful:       {}", successful_trades);
    println!("  Avg latency:      {} ms", avg_latency);
    println!(
        "  Mode:             {}",
        if args.paper {
            "PAPER (real on-chain data, simulated exec)"
        } else if args.dry_run {
            "DRY-RUN"
        } else if args.live {
            "LIVE"
        } else {
            "SIMULATION"
        }
    );
    println!("  Data directory:   {}", args.data_dir.display());
    if args.paper {
        println!("  ── Paper (simulated, never on-chain) ──");
        println!("  Simulated entries:     {}", paper_sim.entries);
        println!("  Exits stop-loss:       {}", paper_sim.exits_stop_loss);
        println!("  Exits take-profit:     {}", paper_sim.exits_take_profit);
        println!(
            "  Exits forced:          {} (max hold / liquidity drop)",
            paper_sim.exits_forced
        );
        println!("  Holds:                 {}", paper_sim.holds);
        println!("  No entry signal:       {}", paper_sim.no_entry_signals);
        println!("  Market-data errors:    {}", paper_data_errors);
        println!("  Rejections:            {}", paper_rejects);
        if !rejected_by_reason.is_empty() {
            let mut reasons: Vec<_> = rejected_by_reason.iter().collect();
            reasons.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
            for (reason, count) in reasons {
                println!("    - {reason}: {count}");
            }
        }
        println!(
            "  Gross P&L (spot move): {} lamports ({:.6} SOL)",
            paper_sim.simulated_pnl_lamports,
            paper_sim.simulated_pnl_lamports as f64 / 1e9
        );
        println!(
            "  Est. costs:            {} lamports (fees, impact, latency, network — estimate)",
            paper_sim.costs_lamports
        );
        println!(
            "  Net P&L (estimate):    {} lamports ({:.6} SOL)",
            paper_sim.net_pnl_lamports,
            paper_sim.net_pnl_lamports as f64 / 1e9
        );
    }
    if risk_manager.is_circuit_breaker_active() {
        println!("  ⚠️  CIRCUIT BREAKER ACTIVE");
    }
    println!("========================================");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(p: &str) -> Option<PathBuf> {
        Some(PathBuf::from(p))
    }

    #[test]
    fn live_requires_hsm() {
        let err =
            validate_signing_config(true, false, false, true, &None, &None, &None).unwrap_err();
        assert!(err.contains("--hsm-endpoint"), "got: {err}");
    }

    #[test]
    fn live_without_confirm_live_is_rejected() {
        let err = validate_signing_config(
            true,
            false,
            false,
            false,
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .unwrap_err();
        assert!(err.contains("--confirm-live"), "got: {err}");
    }

    #[test]
    fn live_with_hsm_requires_mtls_certs() {
        let err = validate_signing_config(
            true,
            false,
            false,
            true,
            &Some("https://127.0.0.1:8443".to_string()),
            &None,
            &None,
        )
        .unwrap_err();
        assert!(err.contains("--hsm-ca"), "got: {err}");
    }

    #[test]
    fn live_with_full_mtls_ok() {
        assert!(validate_signing_config(
            true,
            false,
            false,
            true,
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .is_ok());
    }

    #[test]
    fn live_and_dry_run_conflict() {
        let err = validate_signing_config(
            true,
            true,
            false,
            true,
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn dry_run_without_hsm_ok() {
        assert!(validate_signing_config(false, true, false, false, &None, &None, &None).is_ok());
    }

    #[test]
    fn dry_run_with_hsm_requires_mtls_certs() {
        let err = validate_signing_config(
            false,
            true,
            false,
            false,
            &Some("https://127.0.0.1:8443".to_string()),
            &None,
            &None,
        )
        .unwrap_err();
        assert!(err.contains("--hsm-ca"), "got: {err}");
    }

    #[test]
    fn paper_conflicts_with_live() {
        let err = validate_signing_config(
            true,
            false,
            true,
            true,
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn paper_conflicts_with_dry_run() {
        let err = validate_signing_config(
            false,
            true,
            true,
            false,
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    // ── LIVE prerequisites (pool, real risk data, pinned HSM key) ──
    const PK: &str = "4sirSH9isRCEqcu72TKDXhV1kn6sURKabM981u3ZTvUt";

    #[test]
    fn live_requires_pool_args() {
        let err = validate_live_args(true, None, Some("a"), Some("b"), true, Some(PK)).unwrap_err();
        assert!(err.contains("--pool-id"), "got: {err}");
    }

    #[test]
    fn live_requires_live_risk_data() {
        let err =
            validate_live_args(true, Some("p"), Some("a"), Some("b"), false, Some(PK)).unwrap_err();
        assert!(err.contains("--live-risk-data"), "got: {err}");
    }

    #[test]
    fn live_requires_expected_signer() {
        let err =
            validate_live_args(true, Some("p"), Some("a"), Some("b"), true, None).unwrap_err();
        assert!(err.contains("--expected-signer"), "got: {err}");
    }

    #[test]
    fn live_with_all_prerequisites_ok() {
        let pinned =
            validate_live_args(true, Some("p"), Some("a"), Some("b"), true, Some(PK)).unwrap();
        assert_eq!(pinned.unwrap().to_string(), PK);
    }

    #[test]
    fn invalid_expected_signer_rejected_in_any_mode() {
        assert!(validate_live_args(false, None, None, None, false, Some("nope")).is_err());
        assert!(validate_live_args(false, None, None, None, false, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn exit_slippage_widens_per_failure_and_caps_at_5pct() {
        assert_eq!(exit_slippage_bps(100, 0), 100);
        assert_eq!(exit_slippage_bps(100, 1), 200);
        assert_eq!(exit_slippage_bps(100, 3), 400);
        assert_eq!(exit_slippage_bps(100, 10), 500);
        assert_eq!(exit_slippage_bps(100, u32::MAX), 500);
        // A configured slippage above the cap is never reduced.
        assert_eq!(exit_slippage_bps(800, 2), 800);
    }

    #[test]
    fn max_hold_time_triggers_only_when_enabled_and_elapsed() {
        let opened = 1_000_000;
        assert!(!max_hold_reached(opened, opened + 59_999, 60));
        assert!(max_hold_reached(opened, opened + 60_000, 60));
        assert!(!max_hold_reached(opened, opened + 10_000_000_000, 0));
    }

    #[test]
    fn entry_requires_sol_reserve_on_top_of_size() {
        assert_eq!(entry_required_balance(10_000_000, true), 16_000_000);
        assert_eq!(entry_required_balance(10_000_000, false), 10_000_000);
        // The wallet that failed the dry-run simulation (0.00203 SOL) is
        // rejected up front instead of failing on-chain every iteration.
        assert!(2_028_080 < entry_required_balance(10_000_000, true));
    }

    #[test]
    fn clmm_program_id_by_cluster() {
        assert_eq!(
            clmm_program_id("https://api.devnet.solana.com").to_string(),
            amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID_DEVNET
        );
        assert_eq!(
            clmm_program_id("https://mainnet.helius-rpc.com").to_string(),
            amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID
        );
    }

    #[test]
    fn args_can_be_supplied_via_env() {
        // SNIPER_* env vars feed the same flags (systemd EnvironmentFile).
        std::env::set_var("SNIPER_POOL_ID", "envpool");
        let args = Args::try_parse_from(["solana-sniper"]).unwrap();
        std::env::remove_var("SNIPER_POOL_ID");
        assert_eq!(args.pool_id.as_deref(), Some("envpool"));
        // An explicit flag wins over the environment.
        std::env::set_var("SNIPER_ITERATIONS", "7");
        let args = Args::try_parse_from(["solana-sniper", "--iterations", "3"]).unwrap();
        std::env::remove_var("SNIPER_ITERATIONS");
        assert_eq!(args.iterations, 3);
        // Empty values (template lines like `SNIPER_EXPECTED_SIGNER=`) are unset.
        let args = Args::try_parse_from(["solana-sniper", "--expected-signer", ""])
            .unwrap()
            .normalized();
        assert!(args.expected_signer.is_none());
    }

    #[test]
    fn paper_exit_direction_follows_pool_orientation() {
        // Price token1/token0 rises 20% (sqrt * ~1.0954). With token1 as
        // input (held = token0) that is a gain => take-profit; with token0
        // as input (held = token1) the held token lost value => stop-loss.
        let entry = 1u128 << 64;
        let up = (entry as f64 * 1.2f64.sqrt()) as u128;
        let cand = strategy::TokenCandidate {
            liquidity_lamports: u64::MAX,
            market_cap_lamports: 0,
            holders: 100,
            is_blocklisted: false,
        };
        for (input_is_token0, expect_tp) in [(false, true), (true, false)] {
            let mut sim = PaperSimulator::new();
            sim.input_is_token0 = input_is_token0;
            let PaperTick::EntryPending(sig) = sim.tick(&cand, entry) else {
                panic!("expected entry signal");
            };
            sim.confirm_entry(entry, sig);
            match sim.tick(&cand, up) {
                PaperTick::ClosedTakeProfit(pnl) => assert!(expect_tp && pnl > 0),
                PaperTick::ClosedStopLoss(pnl) => assert!(!expect_tp && pnl < 0),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn paper_alone_ok() {
        assert!(validate_signing_config(false, false, true, false, &None, &None, &None).is_ok());
    }

    // ── AŞAMA 2/5: paper startup validation (no synthetic pools anymore) ──
    #[test]
    fn paper_requires_pool_id() {
        let err = validate_paper_args(None, Some("mint"), Some("mint")).unwrap_err();
        assert!(err.contains("--pool-id"), "got: {err}");
    }

    #[test]
    fn paper_requires_mints_together_with_pool() {
        let err = validate_paper_args(Some("pool"), None, Some("mint")).unwrap_err();
        assert!(err.contains("--input-mint"), "got: {err}");
        let err = validate_paper_args(Some("pool"), Some("mint"), None).unwrap_err();
        assert!(err.contains("--output-mint"), "got: {err}");
    }

    #[test]
    fn paper_with_pool_and_mints_ok() {
        assert!(validate_paper_args(Some("pool"), Some("mint"), Some("mint")).is_ok());
    }

    // ── AŞAMA 4/5: quote-based simulated P&L (same (current/entry)^2 ratio
    //    formula SimpleSnipeStrategy::should_exit uses — pure, no I/O) ──
    #[test]
    fn paper_close_books_net_after_costs() {
        let mut sim = PaperSimulator::new();
        sim.cost_model = sim_quote::PaperCostModel {
            fee_rate_ppm: 2_500,
            exit_impact_pct: 200,
            latency_bps_per_leg: 50,
            network_cost_per_tx_lamports: 10_000,
        };
        let pos = PaperPosition {
            entry_sqrt_price: 1,
            position_size_lamports: 50_000_000,
            entry_liquidity_lamports: 20_000_000_000,
            opened_at_ms: 1_000,
            peak_ratio: strategy::RATIO_SCALE + strategy::RATIO_SCALE / 10,
            trough_ratio: strategy::RATIO_SCALE - strategy::RATIO_SCALE / 50,
        };
        sim.now_ms = 61_000;
        sim.book_close(&pos, 5_000_000, 20_000_000_000);
        let c = sim.last_close.unwrap();
        assert_eq!(c.gross_pnl_lamports, 5_000_000);
        assert_eq!(c.cost.total_lamports, 1_144_064);
        assert_eq!(c.net_pnl_lamports, 5_000_000 - 1_144_064);
        assert_eq!(c.hold_ms, 60_000);
        assert_eq!((c.mfe_bps, c.mae_bps), (1_000, -200));
        assert_eq!(sim.simulated_pnl_lamports, 5_000_000);
        assert_eq!(sim.net_pnl_lamports, 5_000_000 - 1_144_064);
        // A small spot gain can be a net loss.
        sim.book_close(&pos, 1_000_000, 20_000_000_000);
        assert!(sim.last_close.unwrap().net_pnl_lamports < 0);
    }

    #[test]
    fn simulated_pnl_lamports_zero_at_entry_price_or_zero_entry() {
        assert_eq!(
            simulated_pnl_lamports(1_000_000_000, 1_000_000_000, 1_000_000_000),
            0
        );
        assert_eq!(simulated_pnl_lamports(0, 1_000_000_000, 1_000_000_000), 0);
    }

    #[test]
    fn simulated_pnl_lamports_scales_with_squared_price_ratio() {
        // 2x sqrt price => 4x price => +3x position size (positive).
        assert_eq!(
            simulated_pnl_lamports(1_000_000_000, 2_000_000_000, 1_000_000_000),
            3_000_000_000
        );
        // Half sqrt price => 0.25x price => -0.75x position size (negative).
        assert_eq!(
            simulated_pnl_lamports(2_000_000_000, 1_000_000_000, 1_000_000_000),
            -750_000_000
        );
    }

    #[test]
    fn simulated_pnl_edge_cases_never_overflow() {
        let one = 1u128 << 64;
        // Zero entry, zero current, huge and unrepresentable prices.
        assert_eq!(simulated_pnl_lamports(0, one, 1_000_000_000), 0);
        assert_eq!(
            simulated_pnl_lamports(one, 0, 1_000_000_000),
            -1_000_000_000
        );
        assert_eq!(simulated_pnl_lamports(u128::MAX, u128::MAX, u64::MAX), 0);
        assert_eq!(simulated_pnl_lamports(1, u128::MAX, 1_000_000_000), 0);
        assert_eq!(simulated_pnl_lamports(one, one * 1_000, u64::MAX), i64::MAX);
        assert_eq!(simulated_pnl_lamports(one * 2, one, u64::MAX), -i64::MAX);
        // Negative P&L is exact and rounded away from zero.
        assert_eq!(simulated_pnl_lamports(one * 2, one, 3), -3);
        // Same math as the exit decision: a closed stop-loss is never a gain.
        let s = strategy::SimpleSnipeStrategy::new(strategy::StrategyConfig::default());
        let (e, c) = (one * 1_000, one * 970);
        assert_eq!(s.should_exit(e, c), strategy::ExitDecision::StopLoss);
        assert!(simulated_pnl_lamports(e, c, 10_000_000) < -500_000 + 1);
    }

    // ── AŞAMA 3/5: entries increment ONLY on confirm_entry; a rejected /
    //    no-signal iteration is never a simulated success; exits accumulate
    //    into the simulated P&L exactly once ──
    fn rejected_candidate() -> strategy::TokenCandidate {
        // Blocklisted candidates are always rejected by the unchanged
        // strategy (see strategy.rs tests) — deterministic NoEntrySignal.
        strategy::TokenCandidate {
            liquidity_lamports: 0,
            market_cap_lamports: 0,
            holders: 0,
            is_blocklisted: true,
        }
    }

    fn entry_signal(size: u64) -> strategy::EntrySignal {
        strategy::EntrySignal {
            position_size_lamports: size,
            slippage_bps: 100,
            entry_sqrt_price: 0,
        }
    }

    #[test]
    fn paper_simulator_never_counts_unconfirmed_iterations() {
        let mut sim = PaperSimulator::new();
        assert!(!sim.has_open_position());
        let cand = rejected_candidate();
        // No open position + rejected candidate => no entry signal.
        for _ in 0..3 {
            match sim.tick(&cand, 1_000_000_000) {
                PaperTick::NoEntrySignal => {}
                other => panic!("expected NoEntrySignal, got: {other:?}"),
            }
        }
        assert_eq!(sim.entries, 0, "no entry without confirm_entry");
        assert_eq!(sim.simulated_pnl_lamports, 0);
        assert_eq!(sim.no_entry_signals, 3);
        // Hold/open-position iterations never count either.
        sim.confirm_entry(1_000_000_000, entry_signal(1_000_000_000));
        assert_eq!(sim.entries, 1);
        assert!(matches!(sim.tick(&cand, 1_000_000_000), PaperTick::Hold));
        assert_eq!(sim.entries, 1, "hold must not re-count the entry");
        assert_eq!(sim.holds, 1);
    }

    #[test]
    fn paper_simulator_exits_update_pnl_once_and_clear_position() {
        let mut sim = PaperSimulator::new();
        let cand = rejected_candidate();

        sim.confirm_entry(1_000_000_000, entry_signal(1_000_000_000));
        // sqrt price 0.97x => price ratio 0.9409 => ~5.9% loss >= 5% SL.
        let PaperTick::ClosedStopLoss(sl_pnl) = sim.tick(&cand, 970_000_000) else {
            panic!("expected stop-loss close");
        };
        assert!(sl_pnl < 0, "stop-loss P&L must be negative, got {sl_pnl}");
        assert!(!sim.has_open_position(), "position must clear after close");

        sim.confirm_entry(1_000_000_000, entry_signal(1_000_000_000));
        // sqrt price 1.1x => price ratio 1.21 => +21% >= 10% TP.
        let PaperTick::ClosedTakeProfit(tp_pnl) = sim.tick(&cand, 1_100_000_000) else {
            panic!("expected take-profit close");
        };
        assert!(tp_pnl > 0, "take-profit P&L must be positive, got {tp_pnl}");

        assert_eq!(sim.entries, 2);
        assert_eq!(sim.exits_stop_loss, 1);
        assert_eq!(sim.exits_take_profit, 1);
        assert_eq!(
            sim.simulated_pnl_lamports,
            sl_pnl + tp_pnl,
            "accumulator must equal the sum of realized simulated closes"
        );
    }

    // ── Trade-size vs risk-cap: paper never fakes a success on rejection ──
    #[test]
    fn paper_max_spend_exceeded_is_rejection_never_simulated_success() {
        // Regression: pre-fix the strategy default (0.1 SOL) exceeded the
        // production risk cap (0.05 SOL), so the EntryPending gate rejected
        // every paper entry with MaxSpendExceeded. The rejection must never
        // be recorded as a simulated trade: entries/P&L only move in
        // confirm_entry, which the rejected gate never reaches.
        let cfg =
            risk::RiskConfig::production_defaults(std::env::temp_dir().join("paper_ms_gate_test"))
                .unwrap();
        let default_cfg = strategy::StrategyConfig::default();
        // The default is now within the risk cap (startup invariant Ok)…
        assert!(strategy::validate_position_size_invariant(
            default_cfg.max_trade_size_lamports,
            cfg.max_trade_size_lamports
        )
        .is_ok());
        // …while the old 0.1 SOL value is fail-closed rejected by the same
        // invariant main() enforces at startup.
        assert!(strategy::validate_position_size_invariant(
            100_000_000,
            cfg.max_trade_size_lamports
        )
        .is_err());

        // A qualifying candidate yields EntryPending, but without
        // confirm_entry nothing is ever counted as simulated success.
        let mut sim = PaperSimulator::new();
        let cand = strategy::TokenCandidate {
            liquidity_lamports: 2_000_000_000_000,
            market_cap_lamports: 0,
            holders: 200,
            is_blocklisted: false,
        };
        match sim.tick(&cand, 1u128 << 64) {
            PaperTick::EntryPending(sig) => {
                assert!(
                    sig.position_size_lamports <= cfg.max_trade_size_lamports,
                    "paper signal must never exceed the risk cap"
                );
            }
            other => panic!("expected EntryPending, got {other:?}"),
        }
        assert_eq!(sim.entries, 0, "no confirm_entry => no simulated success");
        assert_eq!(sim.simulated_pnl_lamports, 0);
    }

    // ── AŞAMA exit-path: deterministic SL/TP/P&L semantics (offline) ──
    // Entry at sqrt(1.0)=1e9. current_sqrt = 1e9*sqrt(price_ratio) so the
    // squared-ratio formula hits the exact price move under test.

    fn rich_candidate() -> strategy::TokenCandidate {
        strategy::TokenCandidate {
            liquidity_lamports: 2_000_000_000_000,
            market_cap_lamports: 0,
            holders: 200,
            is_blocklisted: false,
        }
    }

    fn entry_at(size: u64) -> strategy::EntrySignal {
        strategy::EntrySignal {
            position_size_lamports: size,
            slippage_bps: 100,
            entry_sqrt_price: 0,
        }
    }

    #[test]
    fn exit_path_stop_loss_at_6pct_closes_negative_pnl_once() {
        let mut sim = PaperSimulator::new();
        let entry_sqrt = 1_000_000_000u128;
        sim.confirm_entry(entry_sqrt, entry_at(1_000_000_000)); // 1 SOL notional
        let current_sqrt = (entry_sqrt as f64 * 0.94f64.sqrt()) as u128; // -6%
        let PaperTick::ClosedStopLoss(pnl) = sim.tick(&rich_candidate(), current_sqrt) else {
            panic!("expected stop-loss close");
        };
        assert!(!sim.has_open_position(), "position must clear");
        assert_eq!(sim.exits_stop_loss, 1);
        assert_eq!(sim.holds, 0);
        // P&L is exactly the quote formula on the same inputs (no drift):
        assert_eq!(
            pnl,
            simulated_pnl_lamports(entry_sqrt, current_sqrt, 1_000_000_000)
        );
        assert!(pnl < 0);
        // f64 sqrt->int rounding may shift the exact pnl by <=2 lamports.
        assert!(
            (pnl - (-60_000_000)).abs() <= 2,
            "-6% on 1 SOL notional, got {pnl}"
        );
        assert_eq!(sim.simulated_pnl_lamports, pnl);
        // Follow-up ticks with no position run the entry path, not exits:
        assert!(matches!(
            sim.tick(&rich_candidate(), current_sqrt),
            PaperTick::EntryPending(_)
        ));
    }

    #[test]
    fn exit_path_take_profit_at_11pct_closes_positive_pnl_once() {
        let mut sim = PaperSimulator::new();
        let entry_sqrt = 1_000_000_000u128;
        sim.confirm_entry(entry_sqrt, entry_at(1_000_000_000));
        let current_sqrt = (entry_sqrt as f64 * 1.11f64.sqrt()) as u128; // +11%
        let PaperTick::ClosedTakeProfit(pnl) = sim.tick(&rich_candidate(), current_sqrt) else {
            panic!("expected take-profit close");
        };
        assert!(!sim.has_open_position());
        assert_eq!(sim.exits_take_profit, 1);
        assert!(
            (pnl - 110_000_000).abs() <= 2,
            "+11% on 1 SOL notional, got {pnl}"
        );
        assert_eq!(sim.simulated_pnl_lamports, pnl);
    }

    #[test]
    fn exit_path_below_thresholds_holds_at_4p9_and_9p9() {
        let mut sim = PaperSimulator::new();
        let entry_sqrt = 1_000_000_000u128;
        sim.confirm_entry(entry_sqrt, entry_at(1_000_000_000));
        // -4.9% (490 bps < 500 SL): hold, position stays open, no P&L.
        let down = (entry_sqrt as f64 * 0.951f64.sqrt()) as u128;
        assert!(matches!(sim.tick(&rich_candidate(), down), PaperTick::Hold));
        assert!(sim.has_open_position());
        assert_eq!(sim.simulated_pnl_lamports, 0);
        // +9.9% (990 bps < 1000 TP): hold.
        let up = (entry_sqrt as f64 * 1.099f64.sqrt()) as u128;
        assert!(matches!(sim.tick(&rich_candidate(), up), PaperTick::Hold));
        assert!(sim.has_open_position());
        assert_eq!(sim.simulated_pnl_lamports, 0);
        assert_eq!(sim.exits_stop_loss + sim.exits_take_profit, 0);
        assert_eq!(sim.holds, 2);
    }

    #[test]
    fn exit_path_reentry_allowed_after_close_with_full_state_reset() {
        let mut sim = PaperSimulator::new();
        let entry_sqrt = 1_000_000_000u128;
        sim.confirm_entry(entry_sqrt, entry_at(500_000_000));
        let down = (entry_sqrt as f64 * 0.94f64.sqrt()) as u128; // -6% SL
        let PaperTick::ClosedStopLoss(pnl) = sim.tick(&rich_candidate(), down) else {
            panic!("expected stop-loss close");
        };
        // After close the SAME candidate may re-enter next iteration.
        assert!(matches!(
            sim.tick(&rich_candidate(), entry_sqrt),
            PaperTick::EntryPending(_)
        ));
        sim.confirm_entry(entry_sqrt, entry_at(500_000_000));
        assert!(sim.has_open_position());
        assert_eq!(sim.entries, 2, "re-entry must count exactly once");
        // P&L accumulates across the closed position only.
        assert_eq!(sim.simulated_pnl_lamports, pnl);
        // Second close at same move adds the same pnl again.
        let PaperTick::ClosedStopLoss(pnl2) = sim.tick(&rich_candidate(), down) else {
            panic!("expected second stop-loss close");
        };
        assert_eq!(pnl2, pnl);
        assert_eq!(sim.simulated_pnl_lamports, 2 * pnl);
        assert_eq!(sim.exits_stop_loss, 2);
    }

    #[test]
    fn exit_path_pnl_record_fields_match_decision_fields() {
        // The persisted decision context records entry_sqrt, current_sqrt
        // and position_size; the P&L must be exactly simulated_pnl_lamports
        // of those three fields (same function the exit path uses).
        let entry_sqrt = 1_000_000_000u128;
        let current_sqrt = (entry_sqrt as f64 * 1.11f64.sqrt()) as u128;
        let size = 750_000_000u64;
        let recorded_pnl = simulated_pnl_lamports(entry_sqrt, current_sqrt, size);
        // Decision fields -> formula (what the paper loop persists as pnl).
        let entry_field = entry_sqrt;
        let current_field = current_sqrt;
        let size_field = size;
        assert_eq!(
            recorded_pnl,
            simulated_pnl_lamports(entry_field, current_field, size_field)
        );
        assert!(
            (recorded_pnl - 82_500_000).abs() <= 2,
            "+11% of 0.75 SOL, got {recorded_pnl}"
        );
    }

    #[test]
    fn exit_path_persisted_audit_fields_reproduce_recorded_pnl() {
        // The decision record now persists entry_sqrt + position_size_lamports
        // (captured from the open position BEFORE tick) next to the realized
        // pnl. The audit trail must be re-verifiable offline: those two
        // fields + current_sqrt must reproduce the recorded pnl exactly via
        // the same quote formula the exit path used.
        let mut sim = PaperSimulator::new();
        let entry_sqrt = 1_000_000_000u128;
        let size = 750_000_000u64;
        assert_eq!(sim.open_position(), None);
        sim.confirm_entry(entry_sqrt, entry_at(size));
        assert_eq!(
            sim.open_position(),
            Some((entry_sqrt, size)),
            "audit fields must be readable before tick"
        );

        let current_sqrt = (entry_sqrt as f64 * 0.94f64.sqrt()) as u128; // -6% SL
        let PaperTick::ClosedStopLoss(pnl) = sim.tick(&rich_candidate(), current_sqrt) else {
            panic!("expected stop-loss close");
        };
        assert_eq!(sim.open_position(), None, "position cleared after close");

        // JSON-recorded fields -> formula reproduces the recorded pnl.
        let persisted_entry_sqrt = entry_sqrt.to_string();
        let persisted_size = size;
        assert_eq!(
            simulated_pnl_lamports(
                persisted_entry_sqrt.parse::<u128>().unwrap(),
                current_sqrt,
                persisted_size
            ),
            pnl
        );
    }

    #[test]
    fn min_liquidity_floor_and_strategy_config() {
        assert!(validate_min_liquidity(1000.0).is_ok());
        assert!(validate_min_liquidity(MIN_LIQUIDITY_FLOOR_SOL).is_ok());
        assert!(validate_min_liquidity(5.0).is_err());
        assert!(validate_min_liquidity(f64::NAN).is_err());
        let args = Args::try_parse_from(["solana-sniper", "--min-liquidity-sol", "50"]).unwrap();
        let cfg = strategy_config(&args);
        assert_eq!(cfg.min_liquidity_lamports, 50_000_000_000);
        assert_eq!(
            cfg.max_trade_size_lamports,
            strategy::StrategyConfig::default().max_trade_size_lamports
        );
        let defaults = Args::try_parse_from(["solana-sniper"]).unwrap();
        assert_eq!(
            strategy_config(&defaults).min_liquidity_lamports,
            strategy::StrategyConfig::default().min_liquidity_lamports
        );
        assert!(!defaults.single_trade);
        assert_eq!(defaults.entry_window_secs, 0);
    }

    #[test]
    fn entry_window_never_abandons_an_open_position() {
        assert!(!entry_window_expired(10_000, 0, false), "0 = unlimited");
        assert!(!entry_window_expired(59, 60, false));
        assert!(entry_window_expired(60, 60, false));
        assert!(!entry_window_expired(10_000, 60, true));
    }

    #[test]
    fn paper_forced_exits_match_live() {
        let mut cand = rejected_candidate();
        cand.liquidity_lamports = 100_000_000_000;

        // Liquidity pulled while the price is flat: forced exit.
        let mut sim = PaperSimulator::new();
        sim.tick(&cand, 1_000_000_000); // records liquidity seen at entry
        sim.confirm_entry(1_000_000_000, entry_signal(1_000_000_000));
        let mut drained = cand.clone();
        drained.liquidity_lamports = 40_000_000_000;
        assert!(matches!(
            sim.tick(&drained, 1_000_000_000),
            PaperTick::ClosedForced(strategy::ExitDecision::LiquidityDrop, _)
        ));
        assert!(!sim.has_open_position());

        // Max holding time.
        let mut sim = PaperSimulator::new();
        sim.max_hold_ms = 60_000;
        sim.now_ms = 1_000;
        sim.tick(&cand, 1_000_000_000);
        sim.confirm_entry(1_000_000_000, entry_signal(1_000_000_000));
        sim.now_ms = 60_999;
        assert!(matches!(sim.tick(&cand, 1_000_000_000), PaperTick::Hold));
        sim.now_ms = 61_000;
        assert!(matches!(
            sim.tick(&cand, 1_000_000_000),
            PaperTick::ClosedForced(strategy::ExitDecision::MaxHoldTime, 0)
        ));
        assert_eq!(sim.exits_forced, 1);
    }

    #[test]
    fn paper_trailing_stop_and_mfe_mae() {
        let mut cand = rejected_candidate();
        cand.liquidity_lamports = 100_000_000_000;
        let e: u128 = 1 << 64;
        let sqrt = |pct: f64| ((e as f64) * (1.0 + pct / 100.0).sqrt()) as u128;
        let trailing = strategy::StrategyConfig {
            trailing_stop_bps: 300,
            trailing_activation_bps: 500,
            ..strategy::StrategyConfig::default()
        };
        let mut sim = PaperSimulator::with_config(trailing);
        sim.tick(&cand, e);
        sim.confirm_entry(e, entry_signal(10_000_000));
        // -2% then +8% (armed), then back to +6%: still above 1.08 * 0.97.
        for pct in [-2.0, 8.0, 6.0] {
            assert!(
                matches!(sim.tick(&cand, sqrt(pct)), PaperTick::Hold),
                "{pct}"
            );
        }
        // +4.5% < 1.08 * 0.97 = 1.0476: trailing stop, in profit.
        match sim.tick(&cand, sqrt(4.5)) {
            PaperTick::ClosedForced(strategy::ExitDecision::TrailingStop, pnl) => assert!(pnl > 0),
            other => panic!("{other:?}"),
        }
        let c = sim.last_close.unwrap();
        assert!((799..=800).contains(&c.mfe_bps), "{}", c.mfe_bps);
        assert!((-200..=-199).contains(&c.mae_bps), "{}", c.mae_bps);

        // Same path with the default config (trailing off): no exit.
        let mut off = PaperSimulator::new();
        off.tick(&cand, e);
        off.confirm_entry(e, entry_signal(10_000_000));
        for pct in [-2.0, 8.0, 6.0, 4.5] {
            assert!(
                matches!(off.tick(&cand, sqrt(pct)), PaperTick::Hold),
                "{pct}"
            );
        }
    }

    #[test]
    fn ttl_slot_hits_only_same_key_within_ttl() {
        let mut c: TtlSlot<u8, &str> = TtlSlot::new(1_000);
        assert_eq!(c.get(&1, 0), None, "empty");
        c.put(1, 10_000, "a");
        assert_eq!(c.get(&1, 10_000), Some("a"));
        assert_eq!(c.get(&1, 10_999), Some("a"));
        assert_eq!(c.get(&1, 11_000), None, "expired");
        assert_eq!(c.get(&2, 10_500), None, "other pool / mints");
        assert_eq!(c.get(&1, 9_000), None, "clock went back: no hit");
        assert_eq!(PAPER_ACCOUNTS_TTL_MS, 300_000);
    }

    #[test]
    fn exit_blocked_needs_an_open_position_and_repeated_failures() {
        assert!(!exit_blocked(false, 100), "no position: nothing to block");
        assert!(!exit_blocked(true, EXIT_BLOCKED_AFTER - 1));
        assert!(exit_blocked(true, EXIT_BLOCKED_AFTER));
        assert!(exit_blocked(true, u32::MAX));
    }

    #[test]
    fn paper_costs_are_booked_exactly_once() {
        let mut cand = rejected_candidate();
        cand.liquidity_lamports = 20_000_000_000;
        let mut sim = PaperSimulator::new();
        sim.cost_model = sim_quote::PaperCostModel {
            fee_rate_ppm: 2_500,
            exit_impact_pct: 200,
            latency_bps_per_leg: 50,
            network_cost_per_tx_lamports: 10_000,
        };
        let e: u128 = 1 << 64;
        sim.tick(&cand, e);
        sim.confirm_entry(e, entry_signal(10_000_000));
        // -6%: stop-loss closes once.
        let down = ((e as f64) * 0.94f64.sqrt()) as u128;
        let gross = match sim.tick(&cand, down) {
            PaperTick::ClosedStopLoss(p) => p,
            other => panic!("{other:?}"),
        };
        let c = sim.last_close.unwrap();
        let parts = c.cost.fee_lamports
            + c.cost.entry_impact_lamports
            + c.cost.exit_impact_lamports
            + c.cost.latency_lamports
            + c.cost.network_lamports;
        assert_eq!(c.cost.total_lamports, parts, "total = sum of its parts");
        assert_eq!(
            c.net_pnl_lamports,
            gross - parts as i64,
            "net = gross - costs, once"
        );
        assert_eq!(sim.net_pnl_lamports, c.net_pnl_lamports);
        assert_eq!(sim.costs_lamports, parts);
        // Further ticks without a position (same or worse price) book nothing.
        for _ in 0..3 {
            sim.tick(&cand, down / 2);
        }
        assert_eq!(sim.net_pnl_lamports, c.net_pnl_lamports);
        assert_eq!(sim.costs_lamports, parts);
        assert_eq!(sim.exits_stop_loss, 1);
    }

    #[test]
    fn runbook_launch_pool_dry_run_command_parses() {
        // docs/VPS_RUNBOOK.md §12 — keep in sync.
        let a = Args::try_parse_from([
            "solana-sniper",
            "--dry-run",
            "--pool-id",
            "11111111111111111111111111111111",
            "--input-mint",
            "So11111111111111111111111111111111111111112",
            "--output-mint",
            "11111111111111111111111111111111",
            "--iterations",
            "3",
            "--signal-gate",
            "false",
        ])
        .unwrap();
        assert!(a.dry_run && !a.live && !a.paper && !a.signal_gate);
        assert_eq!(a.iterations, 3);
    }

    #[test]
    fn trailing_stop_settings_are_validated() {
        assert!(validate_trailing_stop(0, 0).is_ok(), "off");
        assert!(validate_trailing_stop(300, 500).is_ok());
        assert!(validate_trailing_stop(5_000, 500).is_err());
        assert!(validate_trailing_stop(300, 0).is_err());
        let args = Args::try_parse_from(["solana-sniper"]).unwrap();
        assert_eq!(
            (args.trailing_stop_bps, args.trailing_activation_bps),
            (0, 500)
        );
    }

    fn pending_fixture() -> (std::path::PathBuf, position::PendingEntry) {
        let dir = std::env::temp_dir().join(format!(
            "pending_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = position::PendingEntry {
            signature: Signature::default().to_string(),
            blockhash: solana_sdk::hash::Hash::default().to_string(),
            before_in: 100,
            before_out: 0,
            position: position::LivePosition {
                pool_id: Pubkey::new_unique().to_string(),
                input_mint: amm::raydium_v4::NATIVE_MINT.into(),
                output_mint: Pubkey::new_unique().to_string(),
                entry_sqrt_price: "1".into(),
                position_size_lamports: 10_000_000,
                cost_input_units: 0,
                token_amount: 0,
                entry_signature: Signature::default().to_string(),
                opened_at_ms: 1,
                entry_liquidity_lamports: 0,
                peak_value_ratio: 0,
            },
        };
        p.save(&dir).unwrap();
        (dir, p)
    }

    fn mock_rpc_blockhash_expired() -> RpcClient {
        let mut m = solana_rpc_client::mock_sender::MocksMap::default();
        for _ in 0..4 {
            m.insert(
                solana_rpc_client_api::request::RpcRequest::IsBlockhashValid,
                serde_json::json!({"context": {"slot": 1}, "value": false}),
            );
        }
        RpcClient::new_mock_with_mocks_map("sig_not_found", m)
    }

    #[test]
    fn unfinished_entry_that_never_landed_is_dropped() {
        let (dir, _) = pending_fixture();
        let rm =
            risk::RiskManager::new(risk::RiskConfig::production_defaults(dir.clone()).unwrap());
        reconcile_pending_entry(&dir, &mock_rpc_blockhash_expired(), &rm).unwrap();
        assert_eq!(position::PendingEntry::load(&dir).unwrap(), None);
        assert_eq!(position::LivePosition::load(&dir).unwrap(), None);
        assert_eq!(rm.open_position_count(), 0);
    }

    #[test]
    fn unfinished_entry_with_booked_position_only_needs_cleanup() {
        let (dir, p) = pending_fixture();
        let mut pos = p.position.clone();
        pos.token_amount = 5;
        pos.save(&dir).unwrap();
        let rm =
            risk::RiskManager::new(risk::RiskConfig::production_defaults(dir.clone()).unwrap());
        // No RPC call is needed: the mock would fail any balance read.
        reconcile_pending_entry(&dir, &mock_rpc_blockhash_expired(), &rm).unwrap();
        assert_eq!(position::PendingEntry::load(&dir).unwrap(), None);
        assert_eq!(position::LivePosition::load(&dir).unwrap(), Some(pos));
    }

    #[test]
    fn corrupt_pending_entry_refuses_live() {
        let (dir, _) = pending_fixture();
        std::fs::write(dir.join("pending_entry.json"), "{not json").unwrap();
        let rm =
            risk::RiskManager::new(risk::RiskConfig::production_defaults(dir.clone()).unwrap());
        assert!(reconcile_pending_entry(&dir, &mock_rpc_blockhash_expired(), &rm).is_err());
    }

    #[test]
    fn entry_economics_rejects_impact_and_thin_edge() {
        let args = Args::try_parse_from([
            "solana-sniper",
            "--compute-unit-limit",
            "300000",
            "--priority-fee-micro-lamports",
            "10000",
        ])
        .unwrap();
        assert_eq!(args.max_price_impact_bps, 300);
        assert_eq!(args.min_net_edge_bps, 300);
        assert_eq!(args.exit_impact_pct, 200);
        assert_eq!(args.max_position_liquidity_bps, 200);
        assert!(!args.exit_slippage_escalation);
        let liq = Some(20_000_000_000); // 20 SOL
                                        // 1% entry impact, 2% estimated exit impact, 0.25% fee per leg,
                                        // 8 bps network per tx + 1 failed tx: 1000-50-100-200-16-8 = 626.
        let e = entry_economics(&args, 1_000_000, 990_000, 2_500, 10_000_000, 0, liq).unwrap();
        assert_eq!(e.entry_impact_bps, 100);
        assert_eq!(e.exit_impact_bps, 200);
        assert_eq!(e.net_edge_bps, 626);
        assert_eq!(e.worst_case_edge_bps, 626 - 100);
        // 4% impact > 3% cap.
        assert_eq!(
            entry_economics(&args, 1_000_000, 960_000, 2_500, 10_000_000, 0, liq),
            Err("price_impact_exceeded")
        );
        // 2.9% impact passes the cap but leaves too little edge after a 1% fee.
        assert_eq!(
            entry_economics(&args, 1_000_000, 971_000, 10_000, 10_000_000, 0, liq),
            Err("net_edge_too_small")
        );
        // A huge Jito tip on a tiny position kills the edge too.
        assert_eq!(
            entry_economics(&args, 1_000_000, 1_000_000, 2_500, 10_000_000, 400_000, liq),
            Err("net_edge_too_small")
        );
        // Unknown or zero liquidity, or a position too large for it, rejects.
        for l in [None, Some(0)] {
            assert_eq!(
                entry_economics(&args, 1_000_000, 1_000_000, 2_500, 10_000_000, 0, l),
                Err("liquidity_unverified")
            );
        }
        assert_eq!(
            entry_economics(
                &args,
                1_000_000,
                1_000_000,
                2_500,
                50_000_000,
                0,
                Some(1_000_000_000)
            ),
            Err("position_too_large_for_liquidity")
        );
        // Zero position never trades.
        assert!(entry_economics(&args, 1_000_000, 1_000_000, 2_500, 0, 0, liq).is_err());
    }

    #[test]
    fn signal_gate_defaults_on_and_blocks_until_warmed_up() {
        let args = Args::try_parse_from(["solana-sniper"]).unwrap();
        assert!(args.signal_gate);
        assert_eq!(args.signal_min_confidence, 70);
        let cfg = signal::SignalConfig::default();
        let empty = signal::Series::new(10, 1_000);
        assert_eq!(signal_gate_rejects(&args, &empty, &cfg), Some("warming_up"));
        let off = Args::try_parse_from(["solana-sniper", "--signal-gate", "false"]).unwrap();
        assert_eq!(signal_gate_rejects(&off, &empty, &cfg), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn paper_and_dry_run_can_never_broadcast() {
        let rpc = RpcClient::new_mock("succeeds".to_string());
        let tx = Transaction::new_unsigned(solana_sdk::message::Message::new(&[], None));
        for argv in [
            vec!["solana-sniper", "--paper"],
            vec!["solana-sniper", "--dry-run"],
            vec!["solana-sniper", "--live"],
            vec!["solana-sniper"],
        ] {
            let args = Args::try_parse_from(argv.clone()).unwrap();
            let err = submit_live_tx(&args, &rpc, &tx)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("broadcast refused"), "{argv:?}: {err}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_exit_keeps_the_position_and_ignores_entry_gates() {
        let (dir, p) = pending_fixture();
        let mut pos = p.position.clone();
        pos.save(&dir).unwrap();
        let args = Args::try_parse_from(["solana-sniper", "--live"]).unwrap();
        let rpc = Arc::new(RpcClient::new_mock("fails".to_string()));
        let rm =
            risk::RiskManager::new(risk::RiskConfig::production_defaults(dir.clone()).unwrap());
        // Entry gates closed: the exit must still be attempted.
        rm.trigger_kill_switch("test");
        let m = metrics::Metrics::new();
        let bh = std::sync::Mutex::new(retry::BlockhashManager::new(rpc.clone()));
        let out = manage_open_position(
            &args,
            &rpc,
            &rm,
            &m,
            &bh,
            &Pubkey::new_unique(),
            &mut pos,
            None,
            0,
        )
        .await;
        match out {
            ExitOutcome::Failed(e) => assert!(!e.contains("kill"), "{e}"),
            other => panic!("expected a failed exit, got {other:?}"),
        }
        // The position record is untouched: the next iteration retries.
        assert!(position::LivePosition::load(&dir).unwrap().is_some());
    }
}
