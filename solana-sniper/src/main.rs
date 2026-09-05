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

/// Read the raw token balance (u64 `amount` field, little-endian at byte
/// offset 64 of an SPL token account) of the owner's ATA for `mint`.
/// Returns `Ok(None)` when the ATA does not exist yet, `Err` on RPC failure
/// or malformed data (fail-closed callers decide what None means — for a
/// position that should hold tokens, None is treated as zero/absent, never
/// as a fabricated balance).
///
/// NOTE: synchronous RPC (blocking) — matches the codebase convention of
/// using the blocking `RpcClient` inside the tokio runtime (see
/// `PoolPriceFeed::refresh`).
fn token_account_balance_raw(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<Option<u64>, String> {
    let ata = amm::account_resolver::resolve_user_ata(owner, mint);
    let resp = rpc
        .get_account_with_commitment(&ata, CommitmentConfig::confirmed())
        .map_err(|e| format!("failed to fetch token account {ata}: {e}"))?;
    let Some(account) = resp.value else {
        return Ok(None);
    };
    if account.data.len() < 72 {
        return Err(format!(
            "token account {ata} data too short: {} bytes (< 72)",
            account.data.len()
        ));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&account.data[64..72]);
    Ok(Some(u64::from_le_bytes(buf)))
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

        metrics::record_trade_attempt(&metrics_registry);
        metrics::set_risk_gauges(
            &metrics_registry,
            risk_manager.is_circuit_breaker_active(),
            risk_manager.realized_pnl(),
            risk_manager.open_position_count(),
        );
        // Entry gates apply only when no position is open. With an open
        // position this is an EXIT-management iteration: the exit path below
        // enforces its own fail-closed gates (pre_exit_check), and opening a
        // second position is impossible by construction (max 1 open
        // position), so the entry pre_trade_check would only false-positive
        // and halt the loop right after a successful entry.
        if risk_manager.open_position_count() == 0 {
            if let Err(e) =
                risk_manager.pre_trade_check(solana_sdk::native_token::sol_to_lamports(0.01), 50)
            {
                metrics::record_trade_rejected(&metrics_registry, e.code());
                tracing::error!(target: "main", iteration = i, error = %e, "RISK CHECK FAILED — skipping trade");
                if args.live {
                    eprintln!("[CRITICAL] Risk check failed: {}. Stopping.", e);
                    break;
                }
                sleep(Duration::from_millis(200)).await;
                continue;
            }
        }

        if args.paper {
            // Paper-trading: run the strategy on simulated market data. No
            // on-chain transaction is ever built or submitted.
            let strategy = strategy::SimpleSnipeStrategy::new(strategy::StrategyConfig::default());
            // Simulated market state for this iteration (deterministic seed).
            let seed = i as u64;
            let liquidity = 2_000_000_000_000u64 + (seed % 1_000_000_000_000);
            let market_cap = 100_000_000_000_000u64 + (seed % 50_000_000_000_000);
            let holders = 100 + (seed % 400);
            let candidate = strategy::TokenCandidate {
                liquidity_lamports: liquidity,
                market_cap_lamports: market_cap,
                holders,
                is_blocklisted: false,
            };
            // Simulated sqrt price drifts around the entry to exercise exits.
            let entry_sqrt = 1u128 << 64;
            let drift = ((seed % 21) as f64 - 10.0) / 100.0; // -10% .. +10%
            let current_sqrt = ((entry_sqrt as f64) * (1.0 + drift).sqrt()) as u128;

            let entry = strategy.evaluate(&candidate, entry_sqrt);
            let exit = strategy.should_exit(entry_sqrt, current_sqrt);

            let mut rec = decision::DecisionRecord::new("simple_snipe", "paper");
            rec.mode = "paper".to_string();
            rec.pool_id = format!("paper_pool_{i}");
            rec.token_in = "SIM".to_string();
            rec.token_out = "SOL".to_string();
            rec.sqrt_price = current_sqrt.to_string();
            rec.liquidity = liquidity.to_string();
            rec.context = serde_json::json!({
                "entry": entry.is_some(),
                "exit": format!("{exit:?}"),
                "drift_pct": drift * 100.0,
            });
            rec.save(&args.data_dir)?;

            tracing::info!(
                target: "paper",
                iteration = i + 1,
                entry = entry.is_some(),
                exit = ?exit,
                "paper-trading decision recorded (no on-chain tx)"
            );
            successful_trades += 1;
            total_trades += 1;
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
            // Direction metadata hoisted out of the pool block: the confirmed
            // entry needs the real mint pubkeys for on-chain balance
            // measurement (position accounting), and the exit path needs them
            // to build the reversed swap.
            let mut live_input_mint: Option<Pubkey> = None;
            let mut live_output_mint: Option<Pubkey> = None;
            let mut live_program_id: Option<Pubkey> = None;
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
                live_input_mint = Some(input_mint);
                live_output_mint = Some(output_mint);

                // Select the CLMM program id by cluster (devnet vs mainnet).
                let program_id = if args.rpc.contains("devnet") {
                    Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID_DEVNET)
                        .expect("valid devnet program id")
                } else {
                    Pubkey::from_str(amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID)
                        .expect("valid mainnet program id")
                };
                live_program_id = Some(program_id);

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

                // ENTRY-only live risk data (holder concentration, vault
                // liquidity, blocklist). Skipped while a position is open:
                // these fetches gate NEW entries — a mandatory TP/SL exit
                // must not be blocked by an entry-scoring RPC failure.
                if args.live_risk_data && risk_manager.open_position_count() == 0 {
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
                        &accounts.input_vault,
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
                    .with_resolved_pool(resolved)
                    .with_program_id(program_id.to_string())
                    .with_input_mint(input_mint);
                swap_adapter = Some(adapter);
            }

            // ================================================================
            // EXIT MANAGEMENT — position close on TP/SL signal
            // ================================================================
            // An open position makes this an EXIT-management iteration: new
            // entries are impossible until the position is closed (max 1 open
            // position, enforced by pre_trade_check / record_entry). The exit
            // path mirrors the entry path's fail-closed discipline: price
            // freshness, pool re-resolution, slippage/min-output, risk gates,
            // a final pre-send recheck, HSM signing, on-chain confirmation —
            // and only then is the position closed with the REAL measured
            // proceeds and the realized P&L booked.
            if risk_manager.open_position_count() > 0 {
                let pos = risk_manager
                    .current_position()
                    .expect("open_position_count > 0 implies a recorded position");

                // The bot trades one pool per process run; a position opened
                // on another pool cannot be managed here (fail-closed halt).
                let Some(pool_id_str) = &args.pool_id else {
                    tracing::error!(target: "live", "open position but --pool-id is not set — cannot manage exit (fail-closed)");
                    eprintln!("[CRITICAL] Open position exists but --pool-id is not set. Cannot manage exit. Stopping.");
                    break;
                };
                if pool_id_str != &pos.pool_id {
                    tracing::error!(target: "live", position_pool = %pos.pool_id, configured_pool = %pool_id_str, "open position belongs to a different pool than --pool-id — cannot manage exit (fail-closed)");
                    eprintln!(
                        "[CRITICAL] Open position pool != --pool-id. Cannot manage exit. Stopping."
                    );
                    break;
                }

                // Exit signal from the EXISTING SimpleSnipeStrategy (TP/SL
                // thresholds from StrategyConfig — no new strategy invented).
                let strategy =
                    strategy::SimpleSnipeStrategy::new(strategy::StrategyConfig::default());
                let exit_decision = strategy.should_exit(pos.entry_sqrt_price, entry_sqrt);

                let mut rec = decision::DecisionRecord::new("simple_snipe", "live_exit");
                rec.mode = "live".to_string();
                rec.pool_id = pos.pool_id.clone();
                rec.token_in = pos.token_mint.clone();
                rec.token_out = pos.quote_mint.clone();
                rec.amount_in = pos.token_amount_raw;
                rec.sqrt_price = entry_sqrt.to_string();

                if matches!(exit_decision, strategy::ExitDecision::Hold) {
                    tracing::info!(
                        target: "live",
                        iteration = i + 1,
                        exit = ?exit_decision,
                        entry_sqrt = pos.entry_sqrt_price,
                        current_sqrt = entry_sqrt,
                        "position open — no exit signal, holding (no new entry while open)"
                    );
                    rec.context = serde_json::json!({ "exit": "hold" });
                    rec.save(&args.data_dir)?;
                    total_trades += 1;
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }

                // 1) Parse the position's mints; they must be resolvable.
                let token_mint = match Pubkey::from_str(&pos.token_mint) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, "position token mint unparseable (fail-closed)");
                        eprintln!("[CRITICAL] Position token mint unparseable. Stopping.");
                        break;
                    }
                };
                let quote_mint = match Pubkey::from_str(&pos.quote_mint) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, "position quote mint unparseable (fail-closed)");
                        eprintln!("[CRITICAL] Position quote mint unparseable. Stopping.");
                        break;
                    }
                };
                let pool_id = match Pubkey::from_str(pool_id_str) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, "--pool-id unparseable (fail-closed)");
                        eprintln!("[CRITICAL] --pool-id unparseable. Stopping.");
                        break;
                    }
                };
                let Some(program_id) = live_program_id else {
                    tracing::error!(target: "live", "no resolved program id for exit (fail-closed)");
                    eprintln!("[CRITICAL] No resolved program id for exit. Stopping.");
                    break;
                };

                // 2) Exit risk gate (kill switch, circuit breaker, daily loss,
                //    daily trade cap, slippage) — fail-closed.
                if let Err(e) = risk_manager.pre_exit_check(args.max_slippage_bps) {
                    metrics::record_trade_rejected(&metrics_registry, e.code());
                    tracing::warn!(target: "live", iteration = i + 1, error = %e, "pre-exit check failed — exit rejected this iteration (fail-closed)");
                    rec.context = serde_json::json!({
                        "exit": format!("{exit_decision:?}"),
                        "risk_rejected": e,
                    });
                    rec.save(&args.data_dir)?;
                    total_trades += 1;
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }

                // 3) Measure the ACTUAL token balance to sell (raw units).
                //    Fail-closed: unmeasurable or zero balance = no exit (the
                //    position stays recorded-open and no new entry can occur).
                let sell_amount = match token_account_balance_raw(&rpc_client, &from, &token_mint) {
                    Ok(Some(b)) if b > 0 => b,
                    Ok(_) => {
                        tracing::error!(target: "live", token_mint = %token_mint, "exit aborted: token balance is zero or account missing — position may have been moved manually (fail-closed)");
                        risk_manager.trip_circuit_breaker(
                            "exit aborted: zero token balance while position recorded open",
                        );
                        eprintln!("[CRITICAL] Exit aborted: zero token balance while position recorded open. Stopping.");
                        break;
                    }
                    Err(e) => {
                        risk_manager.trip_circuit_breaker(&format!(
                            "token balance measurement failed (fail-closed): {e}"
                        ));
                        tracing::error!(target: "live", error = %e, "token balance measurement failed — circuit breaker tripped");
                        eprintln!("[CRITICAL] Token balance measurement failed. Stopping.");
                        break;
                    }
                };

                // 4) Re-resolve the swap account set for the REVERSED
                //    direction (sell token → quote). Account resolution is
                //    direction-aware: input vault / token account follow the
                //    mint arguments.
                let (accounts, resolved) = match amm::account_resolver::resolve_swap_accounts(
                    &rpc_client,
                    &pool_id,
                    &from,
                    &token_mint,
                    &quote_mint,
                    &program_id,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        risk_manager.trip_circuit_breaker(&format!(
                            "exit account resolution failed (fail-closed): {e}"
                        ));
                        tracing::error!(target: "live", error = %e, "exit account resolution failed — circuit breaker tripped");
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };

                // 5) Direction-aware quote (sell token → quote): the adapter
                //    divides by the pool price when the input is token1.
                let adapter = amm::raydium_v4::RaydiumV4ClmmAdapter::new(pool_id_str.clone())
                    .with_swap_accounts(accounts)
                    .with_resolved_pool(resolved)
                    .with_program_id(program_id.to_string())
                    .with_input_mint(token_mint);
                let quote = match adapter.quote(sell_amount, args.max_slippage_bps) {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, sell_amount, "exit quote failed — no exit this iteration (fail-closed)");
                        rec.context = serde_json::json!({
                            "exit": format!("{exit_decision:?}"),
                            "error": format!("{e}"),
                        });
                        rec.save(&args.data_dir)?;
                        total_trades += 1;
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let intent = match adapter.build_intent(quote) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, "exit intent build failed (fail-closed)");
                        risk_manager.trip_circuit_breaker(&format!(
                            "exit intent build failed (fail-closed): {e}"
                        ));
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let swap_ix = match adapter.build_swap_instruction(&intent, &from) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(target: "live", error = %e, "exit swap instruction failed (fail-closed)");
                        risk_manager.trip_circuit_breaker(&format!(
                            "exit swap instruction failed (fail-closed): {e}"
                        ));
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };

                // 6) Preamble: idempotent ATA creation for both mints so the
                //    proceeds have a destination (the token ATA already
                //    exists from the entry; re-creating is a no-op). No SOL
                //    wrap is needed — we are selling tokens, not wrapping.
                let mut ixs: Vec<solana_sdk::instruction::Instruction> = Vec::new();
                ixs.push(
                    spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                        &from, &from, &token_mint, &spl_token::ID,
                    ),
                );
                ixs.push(
                    spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                        &from, &from, &quote_mint, &spl_token::ID,
                    ),
                );
                ixs.push(swap_ix);

                // 7) Fresh blockhash, then the unsigned exit transaction.
                let blockhash = resolve_blockhash(&args, &blockhash_mgr)?;
                let msg = solana_sdk::message::Message::new(&ixs, Some(&from));
                let mut tx = Transaction::new_unsigned(msg);
                tx.message.recent_blockhash = blockhash;

                // 8) FINAL pre-send recheck — state may have changed (e.g.
                //    the kill switch tripped) since the earlier check.
                if let Err(e) = risk_manager.pre_exit_check(args.max_slippage_bps) {
                    metrics::record_trade_rejected(&metrics_registry, e.code());
                    tracing::warn!(target: "live", iteration = i + 1, error = %e, "final pre-send exit recheck failed — aborting send (fail-closed)");
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }

                // 9) HSM signing (fail-closed: signing failure trips the
                //    circuit breaker and skips the iteration; the position
                //    stays open for a later retry).
                let sig = match hsm_sign(endpoint, ca, identity, &mut tx).await {
                    Ok(s) => s,
                    Err(e) => {
                        risk_manager.trip_circuit_breaker(&format!(
                            "HSM signing failed on exit (fail-closed): {e}"
                        ));
                        tracing::error!(target: "live", error = %e, "exit HSM signing failed — circuit breaker tripped");
                        sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                tx.signatures = vec![sig];

                // 10) Submission via plain RPC retry. Jito bundles are an
                //     entry-side latency optimization; exits prioritize
                //     reliability and simplicity (fail-closed is unchanged).
                match retry::send_with_retry(&*rpc_client, &tx) {
                    Ok(sig) => {
                        // 11) On-chain confirmation: measure the REAL quote
                        //     proceeds (balance delta on the quote ATA) and
                        //     book the realized P&L. Fail-closed: an
                        //     unmeasurable or zero-proceeds outcome does NOT
                        //     close the position (no fabricated P&L).
                        let quote_ata = amm::account_resolver::resolve_user_ata(&from, &quote_mint);
                        let quote_after = match token_account_balance_raw(
                            &rpc_client,
                            &from,
                            &quote_mint,
                        ) {
                            Ok(Some(v)) => v,
                            Ok(None) => 0,
                            Err(e) => {
                                risk_manager.trip_circuit_breaker(&format!(
                                    "post-exit quote balance measurement failed (fail-closed): {e}"
                                ));
                                tracing::error!(target: "live", error = %e, "post-exit quote balance measurement failed — circuit breaker tripped");
                                eprintln!("[CRITICAL] Post-exit quote balance measurement failed. Stopping.");
                                break;
                            }
                        };
                        let proceeds = quote_after.saturating_sub(pos.quote_balance_after_entry);
                        if proceeds == 0 {
                            tracing::error!(target: "live", quote_ata = %quote_ata, "exit confirmed but zero proceeds measured — position NOT closed (fail-closed; operator intervention required)");
                            risk_manager.trip_circuit_breaker(
                                "exit confirmed with zero measured proceeds — refusing to book P&L",
                            );
                            eprintln!("[CRITICAL] Exit confirmed but zero proceeds measured. Position left open. Stopping.");
                            break;
                        }
                        match risk_manager.close_position(proceeds) {
                            Ok(_) => {
                                let pnl = risk_manager.realized_pnl();
                                // Exits are counted separately from entries:
                                // record_exit() never consumes the daily
                                // entry cap and can never trip the defensive
                                // kill switch — a mandatory TP/SL close stays
                                // possible after 5 entries.
                                risk_manager.record_exit();
                                successful_trades += 1;
                                metrics::record_trade_executed(&metrics_registry);
                                metrics::set_risk_gauges(
                                    &metrics_registry,
                                    risk_manager.is_circuit_breaker_active(),
                                    pnl,
                                    risk_manager.open_position_count(),
                                );
                                tracing::info!(
                                    target: "live",
                                    signature = %sig,
                                    exit = ?exit_decision,
                                    sell_amount,
                                    proceeds,
                                    realized_pnl_lamports = pnl,
                                    "exit transaction confirmed — position closed, realized P&L booked"
                                );
                                rec.context = serde_json::json!({
                                    "exit": format!("{exit_decision:?}"),
                                    "sell_amount": sell_amount,
                                    "proceeds_lamports": proceeds,
                                    "realized_pnl_lamports": pnl,
                                    "signature": sig.to_string(),
                                });
                                rec.save(&args.data_dir)?;
                                let cluster = if args.rpc.contains("devnet") {
                                    "devnet"
                                } else {
                                    "mainnet-beta"
                                };
                                println!(
                                    "[LIVE] iter {}: EXIT confirmed: https://explorer.solana.com/tx/{}?cluster={} (pnl={} lamports)",
                                    i + 1,
                                    sig,
                                    cluster,
                                    pnl
                                );
                            }
                            Err(e) => {
                                // close_position failed (should not happen —
                                // the position was open); fail-closed halt.
                                tracing::error!(target: "live", error = %e, "close_position failed after confirmed exit (fail-closed)");
                                eprintln!("[CRITICAL] close_position failed after confirmed exit: {e}. Stopping.");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        // Failed exit: the position stays OPEN — never mark
                        // it closed on a failed transaction (fail-closed).
                        tracing::error!(target: "live", error = %e, "exit transaction failed after retries — position remains open");
                        eprintln!(
                            "[LIVE] iter {}: EXIT TX failed: {} — position stays open",
                            i + 1,
                            e
                        );
                    }
                }
                total_trades += 1;
                sleep(Duration::from_millis(200)).await;
                continue;
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
                metrics::record_trade_rejected(&metrics_registry, "max_spend_exceeded");
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
                metrics::record_trade_rejected(&metrics_registry, e.code());
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

            // Position accounting is denominated in SOL lamports: entry spend,
            // exit proceeds and realized P&L are only comparable when the
            // quote side of the pool IS native SOL. A pool quoted in any other
            // asset cannot be managed by this risk engine — reject the entry
            // fail-closed instead of opening an unmeasurable position.
            if let Some(q_mint) = live_input_mint {
                if q_mint != spl_token::native_mint::ID {
                    metrics::record_trade_rejected(&metrics_registry, "unsupported_quote_mint");
                    tracing::warn!(
                        target: "live",
                        iteration = i + 1,
                        quote_mint = %q_mint,
                        "entry rejected: quote side is not native SOL — P&L cannot be booked in SOL lamports (fail-closed)"
                    );
                    total_trades += 1;
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }
            }

            // Fresh blockhash per iteration to avoid replay
            if let Ok(blockhash) = blockhash_mgr.lock().unwrap().force_refresh() {
                // Build the transaction: a real AMM swap when a pool is
                // configured, otherwise the safe self-transfer test path.
                let mut tx = if let Some(adapter) = &swap_adapter {
                    // Real swap: quote from the resolved on-chain price, apply
                    // slippage (min_amount_out), and build the CLMM swap tx
                    // together with the on-chain preamble: idempotent ATA
                    // creation for both mints, plus a native-SOL wrap +
                    // sync_native when the input side is SOL (the CLMM vault
                    // side holds wrapped SOL, so the input ATA must be funded).
                    let quote = adapter
                        .quote(entry_signal.position_size_lamports, args.max_slippage_bps)
                        .map_err(|e| format!("quote failed (fail-closed): {e}"))?;
                    let intent = adapter
                        .build_intent(quote)
                        .map_err(|e| format!("build_intent failed (fail-closed): {e}"))?;
                    let swap_ix = adapter
                        .build_swap_instruction(&intent, &from)
                        .map_err(|e| format!("build_swap_instruction failed (fail-closed): {e}"))?;
                    let buy_input_mint = live_input_mint.expect("set when swap_adapter is set");
                    let buy_output_mint = live_output_mint.expect("set when swap_adapter is set");
                    let mut ixs: Vec<solana_sdk::instruction::Instruction> = Vec::new();
                    ixs.push(
                        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                            &from,
                            &from,
                            &buy_input_mint,
                            &spl_token::ID,
                        ),
                    );
                    ixs.push(
                        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                            &from,
                            &from,
                            &buy_output_mint,
                            &spl_token::ID,
                        ),
                    );
                    if buy_input_mint == spl_token::native_mint::ID {
                        let input_ata = spl_associated_token_account::get_associated_token_address(
                            &from,
                            &buy_input_mint,
                        );
                        ixs.push(solana_sdk::system_instruction::transfer(
                            &from,
                            &input_ata,
                            entry_signal.position_size_lamports,
                        ));
                        ixs.push(
                            spl_token::instruction::sync_native(&spl_token::ID, &input_ata)
                                .map_err(|e| format!("sync_native failed: {e}"))?,
                        );
                    }
                    ixs.push(swap_ix);
                    let msg = solana_sdk::message::Message::new(&ixs, Some(&from));
                    let mut t = Transaction::new_unsigned(msg);
                    // Apply the freshly-fetched blockhash before signing; otherwise the
                    // transaction is submitted with a zero blockhash and the RPC rejects
                    // it with "Blockhash not found" during simulation.
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
                    metrics::record_trade_rejected(&metrics_registry, e.code());
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
                        // On-chain confirmation: measure the REAL received
                        // token amount and the quote-account balance AFTER the
                        // entry (baseline for the exit proceeds delta).
                        // Fail-closed: if the position cannot be measured it
                        // is NOT recorded as open (never invent amounts), the
                        // circuit breaker is tripped and the loop halts so an
                        // operator investigates before any further entry.
                        let buy_output_mint = match live_output_mint {
                            Some(m) => m,
                            None => {
                                tracing::error!(target: "live", "entry confirmed but output mint unknown — cannot measure position (fail-closed)");
                                risk_manager.trip_circuit_breaker(
                                    "entry confirmed with unknown output mint — position measurement impossible",
                                );
                                eprintln!(
                                    "[CRITICAL] Entry confirmed but output mint unknown. Stopping."
                                );
                                break;
                            }
                        };
                        let buy_input_mint = match live_input_mint {
                            Some(m) => m,
                            None => {
                                tracing::error!(target: "live", "entry confirmed but input mint unknown — cannot measure position (fail-closed)");
                                risk_manager.trip_circuit_breaker(
                                    "entry confirmed with unknown input mint — position measurement impossible",
                                );
                                eprintln!(
                                    "[CRITICAL] Entry confirmed but input mint unknown. Stopping."
                                );
                                break;
                            }
                        };
                        let token_amount_raw = match token_account_balance_raw(
                            &rpc_client,
                            &from,
                            &buy_output_mint,
                        ) {
                            Ok(Some(v)) if v > 0 => v,
                            Ok(_) => {
                                tracing::error!(target: "live", token_mint = %buy_output_mint, "entry confirmed but received token balance is zero — not recording a position (fail-closed)");
                                risk_manager.trip_circuit_breaker(
                                        "entry confirmed with zero received token balance — position measurement impossible",
                                    );
                                eprintln!("[CRITICAL] Entry confirmed with zero received tokens. Stopping.");
                                break;
                            }
                            Err(e) => {
                                risk_manager.trip_circuit_breaker(&format!(
                                        "post-entry token balance measurement failed (fail-closed): {e}"
                                    ));
                                tracing::error!(target: "live", error = %e, "post-entry token balance measurement failed — circuit breaker tripped");
                                eprintln!("[CRITICAL] Post-entry token balance measurement failed. Stopping.");
                                break;
                            }
                        };
                        let quote_balance_after_entry = match token_account_balance_raw(
                            &rpc_client,
                            &from,
                            &buy_input_mint,
                        ) {
                            Ok(Some(v)) => v,
                            Ok(None) => 0,
                            Err(e) => {
                                tracing::error!(target: "live", error = %e, "post-entry quote balance measurement failed — circuit breaker tripped");
                                risk_manager.trip_circuit_breaker(&format!(
                                    "post-entry quote balance measurement failed (fail-closed): {e}"
                                ));
                                eprintln!("[CRITICAL] Post-entry quote balance measurement failed. Stopping.");
                                break;
                            }
                        };
                        let position = risk::OpenPosition {
                            pool_id: args
                                .pool_id
                                .clone()
                                .expect("entry confirmed on a pool implies --pool-id was set"),
                            token_mint: buy_output_mint.to_string(),
                            quote_mint: buy_input_mint.to_string(),
                            token_amount_raw,
                            quote_balance_after_entry,
                            spend_lamports: entry_signal.position_size_lamports,
                            entry_sqrt_price: entry_sqrt,
                            opened_at_ms: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis(),
                            entry_sig: sig.to_string(),
                        };
                        match risk_manager.record_entry(position) {
                            Ok(()) => {
                                risk_manager.record_trade();
                                metrics::record_trade_executed(&metrics_registry);
                                metrics::set_risk_gauges(
                                    &metrics_registry,
                                    risk_manager.is_circuit_breaker_active(),
                                    risk_manager.realized_pnl(),
                                    risk_manager.open_position_count(),
                                );
                                tracing::info!(target: "live", signature = %sig, token_amount_raw, quote_balance_after_entry, "entry transaction confirmed — position recorded (open)");
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
                                // The trade executed on-chain but the position
                                // could not be recorded (e.g. cap reached) —
                                // never leave an unrecorded open position.
                                tracing::error!(target: "live", error = %e, "entry confirmed but position record failed (fail-closed)");
                                risk_manager.trip_circuit_breaker(&format!(
                                    "entry confirmed but record_entry failed: {e}"
                                ));
                                eprintln!("[CRITICAL] Entry confirmed but position record failed. Stopping.");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        // No on-chain confirmation: nothing was spent on a
                        // position. Failed entries are NOT booked as losses
                        // (only realized exit losses feed daily_loss via
                        // close_position) — audit and retry next iteration.
                        tracing::error!(target: "live", error = %e, "transaction failed after retries");
                        eprintln!("[LIVE] iter {}: TX failed: {}", i + 1, e);
                    }
                }
            }
        } else {
            tracing::debug!(target: "sim", iteration = i + 1, "simulation iteration");
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

    let avg_latency = if total_trades > 0 {
        total_latency_ms / total_trades as u128
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
        if args.dry_run {
            "DRY-RUN"
        } else if args.live {
            "LIVE"
        } else {
            "SIMULATION"
        }
    );
    println!("  Data directory:   {}", args.data_dir.display());
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
}
