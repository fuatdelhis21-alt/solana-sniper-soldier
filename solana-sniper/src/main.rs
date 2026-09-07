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

mod integration;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

// Local modules
mod amm;
mod config;
mod decision;
mod discovery;
mod executor;
mod hw_signer;
mod jito;
mod marketdata;
mod metrics;
mod onchain_risk;
mod remote_hsm;
mod retry;
mod risk;
mod strategy;
mod tx;
use amm::AmmAdapter;
use hw_signer::SignerAdapter;
use remote_hsm::RemoteHsmSigner;

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
}

/// Outcome of one paper iteration against real market data.
#[derive(Debug)]
enum PaperTick {
    /// Simulated position closed at the simulated stop-loss (realized pnl).
    ClosedStopLoss(i64),
    /// Simulated position closed at the simulated take-profit (realized pnl).
    ClosedTakeProfit(i64),
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
}

impl PaperSimulator {
    fn new() -> Self {
        Self {
            strategy: strategy::SimpleSnipeStrategy::new(strategy::StrategyConfig::default()),
            position: None,
            entries: 0,
            exits_stop_loss: 0,
            exits_take_profit: 0,
            holds: 0,
            no_entry_signals: 0,
            simulated_pnl_lamports: 0,
        }
    }

    fn has_open_position(&self) -> bool {
        self.position.is_some()
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
        if let Some(pos) = self.position {
            let exit = self
                .strategy
                .should_exit(pos.entry_sqrt_price, current_sqrt_price);
            return match exit {
                strategy::ExitDecision::Hold => {
                    self.holds += 1;
                    PaperTick::Hold
                }
                strategy::ExitDecision::StopLoss => {
                    let pnl = simulated_pnl_lamports(
                        pos.entry_sqrt_price,
                        current_sqrt_price,
                        pos.position_size_lamports,
                    );
                    self.simulated_pnl_lamports += pnl;
                    self.exits_stop_loss += 1;
                    self.position = None;
                    PaperTick::ClosedStopLoss(pnl)
                }
                strategy::ExitDecision::TakeProfit => {
                    let pnl = simulated_pnl_lamports(
                        pos.entry_sqrt_price,
                        current_sqrt_price,
                        pos.position_size_lamports,
                    );
                    self.simulated_pnl_lamports += pnl;
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
        });
        self.entries += 1;
    }
}

/// Quote-based simulated P&L for a closed paper position (no real swap): the
/// price ratio `(current/entry)^2` — the SAME formula
/// `SimpleSnipeStrategy::should_exit` uses — applied to the position size.
/// Negative on stop-loss, positive on take-profit. Always reported/labeled as
/// simulated, never mixed with realized on-chain P&L.
fn simulated_pnl_lamports(
    entry_sqrt_price: u128,
    current_sqrt_price: u128,
    position_size_lamports: u64,
) -> i64 {
    if entry_sqrt_price == 0 {
        return 0;
    }
    let entry = entry_sqrt_price as f64;
    let current = current_sqrt_price as f64;
    let price_ratio = (current / entry) * (current / entry);
    ((price_ratio - 1.0) * position_size_lamports as f64) as i64
}

/// Real market snapshot for one paper iteration — the SAME sources and
/// functions the live path uses: WS feed or `PoolPriceFeed` (pool-state sqrt
/// price + snapshot age), the staleness gate, swap-account resolution (real
/// vault addresses), mint/freeze authority checks and, with `--live-risk-data`,
/// real vault liquidity + holder concentration.
struct PaperMarketSnapshot {
    current_sqrt_price: u128,
    price_timestamp_ms: u128,
    liquidity_lamports: u64,
    market_cap_lamports: u64,
    holders: u64,
    is_blocklisted: bool,
    source: &'static str,
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
    let dummy_user = Pubkey::new_from_array([0u8; 32]);
    let (accounts, _pool) = amm::account_resolver::resolve_swap_accounts(
        rpc_client,
        &pool_id,
        &dummy_user,
        &input_mint,
        &output_mint,
        &program_id,
    )
    .map_err(|e| PaperDataError::Unavailable(format!("swap account resolution failed: {e}")))?;

    // 4) Mint/freeze authority rug-check — same as live: a present mint or
    //    freeze authority rejects the trade (fail-closed).
    for mint in [input_mint, output_mint] {
        match onchain_risk::fetch_mint_authority_risk(rpc_client, &mint) {
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
        let vaults = [accounts.input_vault, accounts.output_vault];
        let stats = onchain_risk::fetch_holder_stats(rpc_client, &input_mint, &vaults)
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
                    mint = %input_mint,
                    top_holder_pct = pct,
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
                    mint = %input_mint,
                    top20_holder_pct = pct,
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
                    mint = %input_mint,
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
        price_timestamp_ms,
        liquidity_lamports,
        market_cap_lamports: args.pool_market_cap,
        holders,
        is_blocklisted,
        source,
    })
}

/// HFT Platform CLI arguments.
#[derive(Parser, Debug)]
#[command(
    name = "solana-sniper",
    about = "Solana HFT Platform — ultra-low-latency trading"
)]
struct Args {
    /// RPC endpoint URL
    #[arg(long, default_value = "https://api.devnet.solana.com")]
    rpc: String,

    /// WebSocket endpoint URL
    #[arg(long, default_value = "wss://api.devnet.solana.com")]
    ws: String,

    /// Path to wallet.json
    #[arg(long, default_value = "./wallet.json")]
    wallet: PathBuf,

    /// Dry-run: build + sign transactions, print them, never send
    #[arg(long, default_value_t = false, conflicts_with = "live")]
    dry_run: bool,

    /// Live mode: actually submit transactions to the network
    #[arg(long, default_value_t = false, conflicts_with = "dry_run")]
    live: bool,

    /// Paper-trading mode: run the strategy on simulated market data, never
    /// submit any on-chain transaction. Mutually exclusive with live/dry-run.
    #[arg(long, default_value_t = false, conflicts_with_all = ["live", "dry_run"])]
    paper: bool,

    /// Number of iterations (simulation mode only)
    #[arg(long, default_value_t = 30)]
    iterations: u32,

    /// Optional blockhash (base58) to use for dry-run. When set, dry-run does
    /// not contact the RPC for a blockhash (deterministic / offline smoke tests).
    #[arg(long)]
    blockhash: Option<String>,

    /// Data directory for decision records, logs, and metrics
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,

    /// Remote HSM endpoint (e.g. https://127.0.0.1:8443). When set, transactions are signed via the remote HSM using mTLS.
    #[arg(long)]
    hsm_endpoint: Option<String>,

    /// CA certificate (PEM) used to verify the HSM server (mTLS)
    #[arg(long)]
    hsm_ca: Option<PathBuf>,

    /// Combined client cert + key PEM presented to the HSM server (mTLS)
    #[arg(long)]
    hsm_client_identity: Option<PathBuf>,

    /// Token candidate: pool liquidity (lamports) for strategy evaluation
    #[arg(long, default_value_t = 0)]
    pool_liquidity: u64,

    /// Token candidate: market cap (lamports) for strategy evaluation
    #[arg(long, default_value_t = 0)]
    pool_market_cap: u64,

    /// Token candidate: number of holders for strategy evaluation
    #[arg(long, default_value_t = 0)]
    pool_holders: u64,

    /// Token candidate: mark as blocklisted (rejects the token)
    #[arg(long, default_value_t = false)]
    pool_blocklisted: bool,

    /// Jito Block Engine endpoint. When set, live submissions go through Jito
    /// bundles with RPC fallback.
    #[arg(long)]
    jito_endpoint: Option<String>,

    /// Jito bundle tip (lamports). Only used when --jito-endpoint is set.
    #[arg(long, default_value_t = 0)]
    jito_tip_lamports: u64,

    /// Jito dry-run: validate the bundle payload but never POST it.
    #[arg(long, default_value_t = false)]
    jito_dry_run: bool,

    /// Raydium CLMM pool to trade on. When set, live mode resolves the pool
    /// state on-chain, feeds the real price into the strategy, and builds a
    /// real AMM swap transaction. When unset, live mode keeps the safe
    /// self-transfer test path.
    #[arg(long)]
    pool_id: Option<String>,

    /// Input token mint (base58) for the swap. Required with --pool-id.
    #[arg(long)]
    input_mint: Option<String>,

    /// Output token mint (base58) for the swap. Required with --pool-id.
    #[arg(long)]
    output_mint: Option<String>,

    /// Max slippage in basis points (1/100 of a percent) for the swap.
    #[arg(long, default_value_t = 100)]
    max_slippage_bps: u64,

    /// Max spend in SOL for a single trade (security cap).
    #[arg(long, default_value_t = 0.01)]
    max_spend_sol: f64,

    /// When set with --pool-id, replaces the static --pool-liquidity /
    /// --pool-holders CLI values with real on-chain data read via RPC
    /// (input vault balance + getTokenLargestAccounts/getTokenSupply).
    /// Fail-closed: an RPC error here halts the loop.
    #[arg(long, default_value_t = false)]
    live_risk_data: bool,

    /// Path to a local blocklist file (one base58 mint per line, `#`
    /// comments allowed). Only used with --live-risk-data. Missing file is
    /// treated as an empty blocklist, not an error.
    #[arg(long)]
    blocklist_file: Option<PathBuf>,

    /// Reject a candidate if the single largest holder (excluding the
    /// pool's own vault) holds more than this percent of supply. Only used
    /// with --live-risk-data.
    #[arg(long, default_value_t = 50.0)]
    max_top_holder_pct: f64,

    /// Optional DexScreener cross-check for logging only. Never used to
    /// gate a trade — a DexScreener failure only logs a warning.
    #[arg(long, default_value_t = false)]
    dexscreener_check: bool,

    /// Optional WebSocket endpoint for real-time `programSubscribe` pool
    /// updates (lower latency than RPC polling). Falls back to the
    /// existing RPC-polled market data hook when unset or not yet
    /// connected.
    #[arg(long)]
    ws_endpoint: Option<String>,

    /// Manual "arm" kill switch for LIVE mode. Fail-closed default: false.
    /// The live trading loop refuses to start unless this is explicitly
    /// passed — a human must opt in every time the process starts in live
    /// mode. Has no effect in --paper or --dry-run.
    #[arg(long, default_value_t = false)]
    confirm_live: bool,
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
        .with_writer(non_blocking)
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
    Keypair::from_bytes(&arr).map_err(|e| e.into())
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
    let mut tx = tx.clone();
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
        blockhash_mgr.lock().unwrap().get_or_refresh()
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
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

    let hsm_configured = args.hsm_endpoint.is_some();

    // The local keyfile is used ONLY for dry-run without a remote HSM. In live
    // mode the remote HSM is mandatory, so the local keyfile is never loaded.
    // In paper mode no signer is loaded at all (no on-chain transaction).
    let local_signer = if args.dry_run && !hsm_configured {
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
    let mut paper_sim = PaperSimulator::new();
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

    'main_loop: for i in 0..args.iterations {
        let iteration_start = std::time::Instant::now();

        metrics::record_trade_attempt(&metrics_registry, mode_label);
        metrics::set_risk_gauges(
            &metrics_registry,
            risk_manager.is_circuit_breaker_active(),
            -(risk_manager.current_daily_loss() as i64),
            risk_manager.open_position_count(),
        );
        if let Err(e) =
            risk_manager.pre_trade_check(solana_sdk::native_token::sol_to_lamports(0.01), 50)
        {
            metrics::record_trade_rejected(&metrics_registry, mode_label, e.code());
            *rejected_by_reason.entry(e.code()).or_insert(0) += 1;
            tracing::error!(target: "main", iteration = i, error = %e, "RISK CHECK FAILED — skipping trade");
            if args.live {
                eprintln!("[CRITICAL] Risk check failed: {}. Stopping.", e);
                break;
            }
            sleep(Duration::from_millis(200)).await;
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
                    sleep(Duration::from_millis(200)).await;
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
                    sleep(Duration::from_millis(200)).await;
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
            let strat_start = std::time::Instant::now();
            let tick = paper_sim.tick(&candidate, snapshot.current_sqrt_price);
            paper_strategy_ms = paper_strategy_ms.saturating_add(strat_start.elapsed().as_millis());

            match tick {
                PaperTick::Hold => {
                    rec.amount_in = 0;
                    rec.context = serde_json::json!({
                        "action": "hold",
                        "entry_sqrt": null,
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
                PaperTick::ClosedStopLoss(pnl) | PaperTick::ClosedTakeProfit(pnl) => {
                    let action = if matches!(tick, PaperTick::ClosedStopLoss(_)) {
                        "exit_stop_loss"
                    } else {
                        "exit_take_profit"
                    };
                    rec.context = serde_json::json!({
                        "action": action,
                        "realized_simulated_pnl_lamports": pnl,
                        "entry_sqrt": null,
                        "current_sqrt": snapshot.current_sqrt_price.to_string(),
                        "source": snapshot.source,
                        "simulated": true,
                    });
                    rec.save(&args.data_dir)?;
                    metrics::set_paper_simulated_pnl(
                        &metrics_registry,
                        paper_sim.simulated_pnl_lamports,
                    );
                    tracing::info!(
                        target: "paper",
                        iteration = i + 1,
                        action = action,
                        pnl_lamports = pnl,
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
                    // Real risk gates BEFORE the entry is simulated (same
                    // order as live): max-spend cap, then pre_trade_check.
                    let max_spend_lamports =
                        solana_sdk::native_token::sol_to_lamports(args.max_spend_sol);
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
                        sleep(Duration::from_millis(200)).await;
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
                        "paper ENTRY simulated on real on-chain data (no on-chain tx — would execute)"
                    );
                }
            }

            total_latency_ms =
                total_latency_ms.saturating_add(iteration_start.elapsed().as_millis());

            // Periodic metrics snapshot (paper included; mode-labeled).
            if total_trades > 0 && total_trades % 10 == 0 {
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

            sleep(Duration::from_millis(200)).await;
            continue;
        }

        if args.dry_run {
            tracing::info!(
                target: "dry_run",
                iteration = i + 1,
                hsm = hsm_configured,
                "DRY-RUN: would build and sign transaction"
            );

            if hsm_configured {
                // Remote HSM backend: derive `from` from the HSM, never a local keyfile.
                let endpoint = args.hsm_endpoint.as_ref().expect("validated");
                let ca = args.hsm_ca.as_ref().expect("validated");
                let identity = args.hsm_client_identity.as_ref().expect("validated");
                let from = hsm_pubkey(endpoint, ca, identity).await?;
                let to = Pubkey::new_from_array([0u8; 32]);
                let ix = solana_sdk::system_instruction::transfer(&from, &to, 1_000_000);
                let blockhash = resolve_blockhash(&args, &blockhash_mgr)?;
                let msg = solana_sdk::message::Message::new(&[ix], Some(&from));
                let mut tx = Transaction::new_unsigned(msg);
                let sig = hsm_sign(endpoint, ca, identity, &mut tx).await?;
                tx.signatures = vec![sig];
                tracing::info!(
                    target: "dry_run",
                    hsm_endpoint = %endpoint,
                    "transaction signed via remote HSM (mTLS)"
                );
                let tx_bytes = bincode::serialize(&tx).unwrap_or_default();
                println!(
                    "[DRY-RUN] iter {}: tx (hex) = {}",
                    i + 1,
                    hex::encode(&tx_bytes)
                );
                println!("[DRY-RUN] iter {}: signature = {}", i + 1, tx.signatures[0]);
                tracing::info!(
                    target: "dry_run",
                    signature = %tx.signatures[0],
                    "dry-run transaction built"
                );
            } else if let Some(ref kp) = local_signer {
                let from = kp.pubkey();
                let to = Pubkey::new_from_array([0u8; 32]);
                let ix = solana_sdk::system_instruction::transfer(&from, &to, 1_000_000);
                let blockhash = resolve_blockhash(&args, &blockhash_mgr)?;
                let msg = solana_sdk::message::Message::new(&[ix], Some(&from));
                let mut tx = Transaction::new_unsigned(msg);
                tx.sign(&[kp], blockhash);
                let tx_bytes = bincode::serialize(&tx).unwrap_or_default();
                println!(
                    "[DRY-RUN] iter {}: tx (hex) = {}",
                    i + 1,
                    hex::encode(&tx_bytes)
                );
                println!("[DRY-RUN] iter {}: signature = {}", i + 1, tx.signatures[0]);
                tracing::info!(
                    target: "dry_run",
                    signature = %tx.signatures[0],
                    "dry-run transaction built"
                );
            }
        } else if args.live {
            tracing::info!(
                target: "live",
                iteration = i + 1,
                "LIVE mode iteration"
            );

            if risk_manager.is_circuit_breaker_active() {
                tracing::error!(target: "main", "CIRCUIT BREAKER ACTIVE — stopping live trading");
                eprintln!("[CRITICAL] Circuit breaker active. Trading halted.");
                break;
            }

            // Live mode is fail-closed: the remote HSM is mandatory (validated
            // above) and the local keyfile is never loaded. Any HSM failure
            // (connection, TLS handshake, client cert, signature, verification)
            // propagates via `?` and halts the trading loop.
            let endpoint = args
                .hsm_endpoint
                .as_ref()
                .expect("live mode requires remote HSM (validated)");
            let ca = args.hsm_ca.as_ref().expect("validated");
            let identity = args.hsm_client_identity.as_ref().expect("validated");
            let from = match hsm_pubkey(endpoint, ca, identity).await {
                Ok(pk) => pk,
                Err(e) => {
                    risk_manager
                        .trip_circuit_breaker(&format!("HSM unavailable (fail-closed): {e}"));
                    tracing::error!(target: "live", error = %e, "HSM pubkey fetch failed — circuit breaker tripped");
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let mut to = from; // self-transfer fallback when no pool is configured

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
                let program_id = if args.rpc.contains("devnet") {
                    Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID_DEVNET)
                        .expect("valid devnet program id")
                } else {
                    Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID)
                        .expect("valid mainnet program id")
                };

                // Market data hook: prefer a fresh WebSocket update (lower
                // latency) when the feed is connected and has data for this
                // pool; otherwise fall back to the fail-closed RPC-polled hook.
                let ws_state = ws_provider
                    .as_ref()
                    .filter(|p| p.is_connected())
                    .and_then(|p| p.get_pool_state(pool_id_str));
                let mut price_timestamp_ms: Option<u128> = None;
                let (pool, used_ws) = if let Some(state) = ws_state {
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
                            tracing::error!(target: "live", error = %e, "failed to resolve pool state — circuit breaker tripped");
                            sleep(Duration::from_millis(200)).await;
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
                    target: "live",
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
                        tracing::warn!(target: "live", iteration = i + 1, reason = %reason, "price staleness check failed — no trade this iteration (fail-closed)");
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                } else {
                    tracing::warn!(target: "live", iteration = i + 1, "no price timestamp available — rejecting trade (fail-closed)");
                    sleep(Duration::from_millis(200)).await;
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
                        tracing::error!(target: "live", error = %e, "failed to resolve swap accounts — circuit breaker tripped");
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };

                // Mint/freeze authority rug-check: always enforced when a
                // pool is configured, independent of --live-risk-data. A
                // present mint or freeze authority means the token issuer
                // can mint more supply or freeze accounts at will — reject
                // fail-closed.
                for (label, mint) in [("input", input_mint), ("output", output_mint)] {
                    match onchain_risk::fetch_mint_authority_risk(&rpc_client, &mint) {
                        Ok(risk) if risk.is_risky() => {
                            tracing::warn!(
                                target: "live",
                                iteration = i + 1,
                                mint_role = label,
                                mint = %mint,
                                mint_authority_present = risk.mint_authority_present,
                                freeze_authority_present = risk.freeze_authority_present,
                                "token authority risk detected — no trade this iteration (fail-closed)"
                            );
                            sleep(Duration::from_millis(200)).await;
                            continue 'main_loop;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            risk_manager.trip_circuit_breaker(&format!(
                                "mint authority check failed (fail-closed): {e}"
                            ));
                            tracing::error!(target: "live", error = %e, mint_role = label, "mint authority check failed — circuit breaker tripped");
                            sleep(Duration::from_millis(200)).await;
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
                            tracing::error!(target: "live", error = %e, "liquidity fetch failed — circuit breaker tripped");
                            sleep(Duration::from_millis(200)).await;
                            continue;
                        }
                    };
                    let holder_stats = match onchain_risk::fetch_holder_stats(
                        &rpc_client,
                        &input_mint,
                        &[accounts.input_vault, accounts.output_vault],
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            risk_manager.trip_circuit_breaker(&format!(
                                "live risk data (fail-closed): {e}"
                            ));
                            tracing::error!(target: "live", error = %e, "holder stats fetch failed — circuit breaker tripped");
                            sleep(Duration::from_millis(200)).await;
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
                                target: "live",
                                verdict = ?verdict,
                                "holder concentration gate rejected candidate (fail-closed)"
                            );
                            metrics::record_trade_rejected(
                                &metrics_registry,
                                mode_label,
                                risk::RejectReason::HolderConcentrationExceeded.code(),
                            );
                            sleep(Duration::from_millis(200)).await;
                            continue;
                        }
                    }
                    tracing::info!(
                        target: "live",
                        liquidity,
                        top_holder_pct = holder_stats.top_holder_pct,
                        sampled_holders = holder_stats.sampled_holders,
                        "fetched real on-chain liquidity + holder concentration"
                    );
                    live_blocklisted =
                        blocklist.contains(&input_mint) || blocklist.contains(&output_mint);
                    live_liquidity = Some(liquidity);
                    live_holder_stats = Some(holder_stats);
                }

                if args.dexscreener_check {
                    // Advisory only — never gates the trade. A failure here
                    // is only logged as a warning.
                    match discovery::fetch_snapshot(pool_id_str) {
                        Ok(snapshot) => tracing::info!(
                            target: "live",
                            liquidity_usd = snapshot.liquidity_usd,
                            fdv = snapshot.fdv,
                            volume_24h_usd = snapshot.volume_24h_usd,
                            "dexscreener advisory snapshot (not used for trading gate)"
                        ),
                        Err(e) => tracing::warn!(
                            target: "live",
                            error = %e,
                            "dexscreener advisory check failed — ignored, not gating trade"
                        ),
                    }
                }

                let adapter = amm::raydium_v4::RaydiumV4ClmmAdapter::new(pool_id_str.clone())
                    .with_swap_accounts(accounts)
                    .with_resolved_pool(resolved);
                swap_adapter = Some(adapter);
            }

            // Strategy gate: evaluate the token candidate. If the strategy
            // rejects it (fail-closed), no trade is built or sent this
            // iteration. This wires SimpleSnipeStrategy into the live path.
            let strategy = strategy::SimpleSnipeStrategy::new(strategy::StrategyConfig::default());
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

            let mut rec = decision::DecisionRecord::new("simple_snipe", "live");
            rec.mode = "live".to_string();
            rec.pool_id = "live_pool".to_string();
            rec.token_in = "SOL".to_string();
            rec.token_out = "SOL".to_string();
            rec.liquidity = candidate.liquidity_lamports.to_string();
            rec.context = serde_json::json!({
                "entry": entry_signal.is_some(),
                "market_cap": candidate.market_cap_lamports,
                "holders": candidate.holders,
                "blocklisted": candidate.is_blocklisted,
            });

            let Some(entry_signal) = entry_signal else {
                tracing::warn!(
                    target: "live",
                    iteration = i + 1,
                    liquidity = candidate.liquidity_lamports,
                    market_cap = candidate.market_cap_lamports,
                    holders = candidate.holders,
                    blocklisted = candidate.is_blocklisted,
                    "strategy rejected candidate — no trade this iteration (fail-closed)"
                );
                rec.save(&args.data_dir)?;
                total_trades += 1;
                sleep(Duration::from_millis(200)).await;
                continue;
            };
            rec.amount_in = entry_signal.position_size_lamports;
            rec.save(&args.data_dir)?;

            // Security: max spend (SOL) cap enforced before any trade.
            let max_spend_lamports = solana_sdk::native_token::sol_to_lamports(args.max_spend_sol);
            if entry_signal.position_size_lamports > max_spend_lamports {
                tracing::warn!(
                    target: "live",
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
                sleep(Duration::from_millis(200)).await;
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
                    target: "live",
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
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            // Fresh blockhash per iteration to avoid replay
            if let Ok(blockhash) = blockhash_mgr.lock().unwrap().force_refresh() {
                // Build the transaction: a real AMM swap when a pool is
                // configured, otherwise the safe self-transfer test path.
                let mut tx = if let Some(adapter) = &swap_adapter {
                    // Real swap: quote from the resolved on-chain price, apply
                    // slippage (min_amount_out), and build the CLMM swap tx.
                    let quote = adapter
                        .quote(entry_signal.position_size_lamports, args.max_slippage_bps)
                        .map_err(|e| format!("quote failed (fail-closed): {e}"))?;
                    let intent = adapter
                        .build_intent(quote)
                        .map_err(|e| format!("build_intent failed (fail-closed): {e}"))?;
                    let mut t = adapter
                        .build_transaction(&intent, &from, blockhash)
                        .map_err(|e| format!("build_transaction failed (fail-closed): {e}"))?;
                    t.message.recent_blockhash = blockhash;
                    t
                } else {
                    // Dynamic compute units per iteration for uniqueness
                    let cu_limit = 400 + (i % 200);
                    let cu_ix =
                        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
                            cu_limit,
                        );
                    let transfer_ix = solana_sdk::system_instruction::transfer(&from, &to, 1_000);
                    let msg = solana_sdk::message::Message::new(&[cu_ix, transfer_ix], Some(&from));
                    let mut t = Transaction::new_unsigned(msg);
                    // Apply the freshly-fetched blockhash before signing; otherwise the
                    // transaction is submitted with a zero blockhash and the RPC rejects
                    // it with "Blockhash not found" during simulation.
                    t.message.recent_blockhash = blockhash;
                    t
                };
                // Final pre-send recheck: re-validate kill switch, circuit
                // breaker, daily limits, slippage, max-spend, and position
                // caps immediately before signing — state may have changed
                // (e.g. another iteration tripped the breaker) since the
                // earlier check at the top of this iteration.
                if let Err(e) = risk_manager.pre_trade_check(
                    entry_signal.position_size_lamports,
                    entry_signal.slippage_bps,
                ) {
                    metrics::record_trade_rejected(&metrics_registry, mode_label, e.code());
                    tracing::warn!(target: "live", iteration = i + 1, error = %e, "final pre-send risk recheck failed — aborting send (fail-closed)");
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }

                let sig = match hsm_sign(endpoint, ca, identity, &mut tx).await {
                    Ok(s) => s,
                    Err(e) => {
                        risk_manager.trip_circuit_breaker(&format!(
                            "HSM signing failed (fail-closed): {e}"
                        ));
                        tracing::error!(target: "live", error = %e, "HSM signing failed — circuit breaker tripped");
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                tx.signatures = vec![sig];

                // Submission: if a Jito endpoint is configured, send via Jito
                // bundle with RPC fallback. Otherwise send directly via RPC.
                //
                // IMPORTANT: in Jito dry-run mode the bundle is validated but
                // never POSTed, so the transaction must still be submitted to
                // the RPC for confirmation. Only a real (non-dry-run) Jito
                // bundle acceptance counts as a confirmed submission.
                let send_result = if let Some(jito_ep) = &args.jito_endpoint {
                    let bundle = jito::JitoBundle::new(vec![tx.clone()], args.jito_tip_lamports);
                    let client = jito::JitoClient::new(jito_ep, args.jito_dry_run);
                    if args.jito_dry_run {
                        // Dry-run: validate the bundle, then submit via RPC.
                        match client.send_bundle(&bundle).await {
                            Ok(bundle_id) => {
                                tracing::info!(target: "live", bundle_id = %bundle_id, "jito bundle dry-run validated — submitting via RPC");
                                retry::send_with_retry(&*rpc_client, &tx)
                            }
                            Err(e) => {
                                tracing::warn!(target: "live", error = %e, "jito dry-run validation failed — submitting via RPC");
                                retry::send_with_retry(&*rpc_client, &tx)
                            }
                        }
                    } else {
                        // Live Jito: send the bundle; fall back to RPC on failure.
                        match client.send_bundle(&bundle).await {
                            Ok(bundle_id) => {
                                tracing::info!(target: "live", bundle_id = %bundle_id, "jito bundle accepted");
                                Ok(tx.signatures[0])
                            }
                            Err(e) => {
                                tracing::warn!(target: "live", error = %e, "jito bundle failed — falling back to RPC");
                                jito::send_with_rpc_fallback(&*rpc_client, &[tx.clone()]).await
                            }
                        }
                    }
                } else {
                    retry::send_with_retry(&*rpc_client, &tx)
                };

                match send_result {
                    Ok(sig) => {
                        successful_trades += 1;
                        // Record the completed trade (daily trade counter)
                        // and the newly opened position/exposure.
                        //
                        // KNOWN GAP: this codebase has no exit-execution
                        // path (SimpleSnipeStrategy::should_exit() is
                        // computed but never acted on in the live loop), so
                        // record_position_close() is never called and
                        // realized P&L is not tracked for wins. See final
                        // report for details — not fabricated here.
                        risk_manager.record_trade();
                        risk_manager.record_position_open(entry_signal.position_size_lamports);
                        metrics::record_trade_executed(&metrics_registry, mode_label);
                        tracing::info!(target: "live", signature = %sig, "transaction confirmed");
                        let cluster = if args.rpc.contains("devnet") {
                            "devnet"
                        } else {
                            "mainnet-beta"
                        };
                        println!(
                            "[LIVE] iter {}: TX confirmed: https://explorer.solana.com/tx/{}?cluster={}",
                            i + 1,
                            sig,
                            cluster
                        );
                    }
                    Err(e) => {
                        let _ = risk_manager.record_loss(1_000);
                        tracing::error!(target: "live", error = %e, "transaction failed after retries");
                        eprintln!("[LIVE] iter {}: TX failed: {}", i + 1, e);
                    }
                }
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

        if total_trades % 10 == 0 {
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

        sleep(Duration::from_millis(200)).await;
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
    println!("  Total iterations: {}", args.iterations);
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
            "  Simulated P&L:         {} lamports ({:.6} SOL)",
            paper_sim.simulated_pnl_lamports,
            paper_sim.simulated_pnl_lamports as f64 / 1e9
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
}
