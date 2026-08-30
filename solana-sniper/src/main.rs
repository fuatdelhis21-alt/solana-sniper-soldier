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
mod executor;
mod hw_signer;
mod jito;
mod metrics;
mod remote_hsm;
mod retry;
mod risk;
mod strategy;
mod tx;
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

    let risk_cfg = risk::RiskConfig::devnet_defaults(args.data_dir.clone());
    let risk_manager = Arc::new(risk::RiskManager::new(risk_cfg));
    tracing::info!(
        target: "main",
        daily_loss = risk_manager.current_daily_loss(),
        circuit_breaker = risk_manager.is_circuit_breaker_active(),
        "risk manager initialized"
    );

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

    for i in 0..args.iterations {
        let iteration_start = std::time::Instant::now();

        if let Err(e) =
            risk_manager.pre_trade_check(solana_sdk::native_token::sol_to_lamports(0.01), 50)
        {
            tracing::error!(target: "main", iteration = i, error = %e, "RISK CHECK FAILED — skipping trade");
            if args.live {
                eprintln!("[CRITICAL] Risk check failed: {}. Stopping.", e);
                break;
            }
            sleep(Duration::from_millis(200)).await;
            continue;
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
            let from = hsm_pubkey(endpoint, ca, identity).await?;
            let to = from; // self-transfer for test

            // Strategy gate: evaluate the token candidate. If the strategy
            // rejects it (fail-closed), no trade is built or sent this
            // iteration. This wires SimpleSnipeStrategy into the live path.
            let strategy = strategy::SimpleSnipeStrategy::new(strategy::StrategyConfig::default());
            let candidate = strategy::TokenCandidate {
                liquidity_lamports: args.pool_liquidity,
                market_cap_lamports: args.pool_market_cap,
                holders: args.pool_holders,
                is_blocklisted: args.pool_blocklisted,
            };
            let entry_sqrt = 1u128 << 64; // placeholder until real pool price is fetched
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

            // Risk gate: enforce kill switch, daily trade cap, position size,
            // and slippage before building any transaction. Fail-closed: if
            // any limit is exceeded, no trade is built or sent this iteration.
            if let Err(e) = risk_manager.pre_trade_check(
                entry_signal.position_size_lamports,
                entry_signal.slippage_bps,
            ) {
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
                // Dynamic compute units per iteration for uniqueness
                let cu_limit = 400 + (i % 200);
                let cu_ix =
                    solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
                        cu_limit,
                    );
                let transfer_ix = solana_sdk::system_instruction::transfer(&from, &to, 1_000);
                let msg = solana_sdk::message::Message::new(&[cu_ix, transfer_ix], Some(&from));
                let mut tx = Transaction::new_unsigned(msg);
                // Apply the freshly-fetched blockhash before signing; otherwise the
                // transaction is submitted with a zero blockhash and the RPC rejects
                // it with "Blockhash not found" during simulation.
                tx.message.recent_blockhash = blockhash;
                let sig = hsm_sign(endpoint, ca, identity, &mut tx).await?;
                tx.signatures = vec![sig];

                // Submission: if a Jito endpoint is configured, send via Jito
                // bundle with RPC fallback. Otherwise send directly via RPC.
                let send_result = if let Some(jito_ep) = &args.jito_endpoint {
                    let bundle = jito::JitoBundle::new(vec![tx.clone()], args.jito_tip_lamports);
                    let client = jito::JitoClient::new(jito_ep, args.jito_dry_run);
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
                } else {
                    retry::send_with_retry(&*rpc_client, &tx)
                };

                match send_result {
                    Ok(sig) => {
                        successful_trades += 1;
                        // Record the completed trade (daily trade counter).
                        risk_manager.record_trade();
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
        let err = validate_signing_config(true, false, false, &None, &None, &None).unwrap_err();
        assert!(err.contains("--hsm-endpoint"), "got: {err}");
    }

    #[test]
    fn live_with_hsm_requires_mtls_certs() {
        let err = validate_signing_config(
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
    fn live_with_full_mtls_ok() {
        assert!(validate_signing_config(
            true,
            false,
            false,
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
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn dry_run_without_hsm_ok() {
        assert!(validate_signing_config(false, true, false, &None, &None, &None).is_ok());
    }

    #[test]
    fn dry_run_with_hsm_requires_mtls_certs() {
        let err = validate_signing_config(
            false,
            true,
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
            &Some("https://127.0.0.1:8443".to_string()),
            &opt("ca.pem"),
            &opt("client_all.pem"),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn paper_alone_ok() {
        assert!(validate_signing_config(false, false, true, &None, &None, &None).is_ok());
    }
}
