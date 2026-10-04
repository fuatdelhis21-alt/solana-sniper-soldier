//! New-pool discovery & auto-snipe supervisor.
//!
//! Listens to Raydium CLMM program logs over a WebSocket `logsSubscribe`
//! feed, detects freshly-created pools (`CreatePool` instruction), runs a
//! fast on-chain pre-filter (mint/freeze authority + liquidity), and — for
//! candidates that pass — spawns the existing, already-proven `solana-sniper`
//! binary as an isolated child process pointed at that pool.
//!
//! Design goals:
//! - **Zero risk to the production bot.** This binary never touches the
//!   live `solana-sniper-soldier` service, its data dir, or its wallet. It
//!   is a separate binary, separate systemd unit, separate data dir, and
//!   (strongly recommended) a separate wallet.
//! - **No duplicated trading logic.** All entry/exit/risk-gate logic
//!   (mint authority, holder concentration, liquidity, circuit breaker,
//!   daily loss cap, TP/SL) lives in `solana-sniper` already and is
//!   battle-tested in production — this binary reuses it by spawning it,
//!   not by re-implementing it.
//! - **Fail-closed.** Any RPC/parse error on a candidate just drops that
//!   candidate; it never crashes the supervisor loop.

use clap::Parser;
use serde::{Deserialize, Serialize};
use solana_pubsub_client::pubsub_client::PubsubClient;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sniper::amm::account_resolver;
use solana_sniper::onchain_risk;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(
    name = "solana-sniper-discovery",
    about = "New Raydium CLMM pool discovery + auto-snipe supervisor"
)]
struct Args {
    /// RPC endpoint URL (http/https) — used for account/transaction reads.
    #[arg(long, env = "RPC_URL")]
    rpc: String,

    /// WebSocket endpoint URL (ws/wss) — used for logsSubscribe.
    #[arg(long, env = "WS_URL")]
    ws: String,

    /// Path to the already-built solana-sniper binary to spawn per candidate.
    #[arg(long, default_value = "./target/release/solana-sniper")]
    sniper_bin: PathBuf,

    /// Wallet used by spawned children (should be a DEDICATED discovery
    /// wallet, never the production bot's wallet).
    #[arg(long, env = "DISCOVERY_WALLET_PATH", default_value = "./wallet.json")]
    wallet: PathBuf,

    /// Remote HSM endpoint passed through to spawned children, if any.
    #[arg(long)]
    hsm_endpoint: Option<String>,
    #[arg(long)]
    hsm_ca: Option<PathBuf>,
    #[arg(long)]
    hsm_client_identity: Option<PathBuf>,

    /// Base data directory. Each spawned child gets its own subdirectory
    /// (`<data_dir>/<pool_id>`) so risk state / audit logs never collide.
    #[arg(long, default_value = "./data-discovery")]
    data_dir: PathBuf,

    /// Comma-separated base58 mints exempt from the authority rug-check,
    /// passed through to spawned children.
    #[arg(long, default_value = "")]
    trusted_mints: String,

    #[arg(long, default_value_t = 100)]
    max_slippage_bps: u64,

    /// Max SOL spent per discovered pool (security cap per child).
    #[arg(long, default_value_t = 0.01)]
    max_spend_sol: f64,

    /// How many strategy iterations a spawned child runs for one pool
    /// before exiting on its own (bounds worst-case lifetime per candidate).
    #[arg(long, default_value_t = 20_000)]
    per_pool_iterations: u32,

    /// Max number of spawned children running concurrently.
    #[arg(long, default_value_t = 1)]
    max_concurrent: usize,

    /// Hard daily cap on how many NEW pools this supervisor will ever enter
    /// (independent of each child's own internal daily-trade cap).
    #[arg(long, default_value_t = 5)]
    max_daily_new_pool_trades: u32,

    /// Pre-filter: reject a candidate pool below this input-vault liquidity
    /// (in whole SOL) before even spawning a child for it.
    #[arg(long, default_value_t = 1.0)]
    min_liquidity_sol: f64,

    /// Pass-through: spawn children in --dry-run instead of --live.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Pass-through: required for children to actually submit live trades.
    #[arg(long, default_value_t = false)]
    confirm_live: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct DailyState {
    date: String,
    entries_today: u32,
}

fn today_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    format!("epoch-day-{days}")
}

fn load_daily_state(path: &std::path::Path) -> DailyState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<DailyState>(&s).ok())
        .filter(|s| s.date == today_str())
        .unwrap_or_else(|| DailyState {
            date: today_str(),
            entries_today: 0,
        })
}

fn save_daily_state(path: &std::path::Path, state: &DailyState) {
    if let Ok(s) = serde_json::to_string(state) {
        let _ = std::fs::write(path, s);
    }
}

/// Fetch a transaction and extract (pool_id, mint_a, mint_b) from a Raydium
/// CLMM `CreatePool` instruction. Account ordering follows the raydium-clmm
/// `create_pool` IDL: [pool_creator, amm_config, pool_state, token_mint_0,
/// token_mint_1, token_vault_0, token_vault_1, observation_state, ...].
/// Fail-closed: any unexpected shape is an error, never a guess.
fn resolve_candidate_from_signature(
    rpc: &RpcClient,
    signature: &str,
    program_id: &Pubkey,
) -> Result<(Pubkey, Pubkey, Pubkey), String> {
    use solana_transaction_status::UiTransactionEncoding;
    let sig = solana_sdk::signature::Signature::from_str(signature)
        .map_err(|e| format!("invalid signature {signature}: {e}"))?;
    let tx = rpc
        .get_transaction(&sig, UiTransactionEncoding::Base64)
        .map_err(|e| format!("getTransaction failed for {signature}: {e}"))?;
    let meta = tx
        .transaction
        .meta
        .ok_or_else(|| "transaction has no meta".to_string())?;
    if meta.err.is_some() {
        return Err("transaction failed on-chain — not a real pool creation".to_string());
    }
    let decoded = tx
        .transaction
        .transaction
        .decode()
        .ok_or_else(|| "failed to decode transaction envelope".to_string())?;
    let account_keys = decoded.message.static_account_keys();
    let instructions = decoded.message.instructions();
    for ix in instructions {
        let prog_idx = ix.program_id_index as usize;
        let Some(prog_key) = account_keys.get(prog_idx) else {
            continue;
        };
        if prog_key != program_id {
            continue;
        }
        if ix.accounts.len() < 6 {
            continue;
        }
        // create_pool account order (raydium-clmm IDL): 0=pool_creator,
        // 1=amm_config, 2=pool_state, 3=token_mint_0, 4=token_mint_1, ...
        let idx = |n: usize| -> Result<Pubkey, String> {
            let a = *ix
                .accounts
                .get(n)
                .ok_or_else(|| format!("instruction missing account index {n}"))?;
            account_keys
                .get(a as usize)
                .copied()
                .ok_or_else(|| format!("account index {a} out of range"))
        };
        let pool_state = idx(2)?;
        let mint0 = idx(3)?;
        let mint1 = idx(4)?;
        return Ok((pool_state, mint0, mint1));
    }
    Err("no CreatePool-shaped instruction for this program found in transaction".to_string())
}

fn reap_finished(children: &mut Vec<(Pubkey, Child)>) {
    children.retain_mut(|(pool_id, child)| match child.try_wait() {
        Ok(Some(status)) => {
            tracing::info!(pool = %pool_id, ?status, "discovery child exited");
            false
        }
        Ok(None) => true,
        Err(e) => {
            tracing::warn!(pool = %pool_id, error = %e, "failed to poll discovery child — assuming still running");
            true
        }
    });
}

fn spawn_child(
    args: &Args,
    pool_id: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
) -> std::io::Result<Child> {
    let child_data_dir = args.data_dir.join(pool_id.to_string());
    let _ = std::fs::create_dir_all(&child_data_dir);
    let mut cmd = Command::new(&args.sniper_bin);
    cmd.arg("--rpc")
        .arg(&args.rpc)
        .arg("--ws")
        .arg(&args.ws)
        .arg("--wallet")
        .arg(&args.wallet)
        .arg("--data-dir")
        .arg(&child_data_dir)
        .arg("--pool-id")
        .arg(pool_id.to_string())
        .arg("--input-mint")
        .arg(input_mint.to_string())
        .arg("--output-mint")
        .arg(output_mint.to_string())
        .arg("--max-slippage-bps")
        .arg(args.max_slippage_bps.to_string())
        .arg("--max-spend-sol")
        .arg(args.max_spend_sol.to_string())
        .arg("--iterations")
        .arg(args.per_pool_iterations.to_string())
        .arg("--live-risk-data")
        .arg("--trusted-mints")
        .arg(&args.trusted_mints)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if args.dry_run {
        cmd.arg("--dry-run");
    } else {
        cmd.arg("--live");
        if args.confirm_live {
            cmd.arg("--confirm-live");
        }
    }
    if let Some(ep) = &args.hsm_endpoint {
        cmd.arg("--hsm-endpoint").arg(ep);
    }
    if let Some(ca) = &args.hsm_ca {
        cmd.arg("--hsm-ca").arg(ca);
    }
    if let Some(ci) = &args.hsm_client_identity {
        cmd.arg("--hsm-client-identity").arg(ci);
    }
    cmd.spawn()
}

fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    std::fs::create_dir_all(&args.data_dir).expect("failed to create discovery data dir");

    let program_id = if args.rpc.contains("devnet") {
        Pubkey::from_str(account_resolver::RAYDIUM_CLMM_PROGRAM_ID_DEVNET)
    } else {
        Pubkey::from_str(account_resolver::RAYDIUM_CLMM_PROGRAM_ID)
    }
    .expect("valid CLMM program id");

    let rpc = RpcClient::new(args.rpc.clone());
    let trusted: HashSet<Pubkey> = args
        .trusted_mints
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| Pubkey::from_str(s.trim()).ok())
        .collect();

    let daily_state_path = args.data_dir.join("discovery_daily_state.json");
    let mut daily_state = load_daily_state(&daily_state_path);
    let mut seen: HashSet<Pubkey> = HashSet::new();
    let mut children: Vec<(Pubkey, Child)> = Vec::new();

    tracing::info!(
        program_id = %program_id,
        ws = %args.ws,
        max_concurrent = args.max_concurrent,
        max_daily = args.max_daily_new_pool_trades,
        "discovery supervisor starting — watching for new Raydium CLMM pools"
    );

    loop {
        let (_sub, receiver) = match PubsubClient::logs_subscribe(
            &args.ws,
            RpcTransactionLogsFilter::Mentions(vec![program_id.to_string()]),
            RpcTransactionLogsConfig {
                commitment: Some(CommitmentConfig::confirmed()),
            },
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "logsSubscribe failed — retrying in 3s");
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        tracing::info!("logsSubscribe connected");

        loop {
            let msg = match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(m) => m,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    reap_finished(&mut children);
                    if daily_state.date != today_str() {
                        daily_state = DailyState {
                            date: today_str(),
                            entries_today: 0,
                        };
                        save_daily_state(&daily_state_path, &daily_state);
                    }
                    continue;
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("logsSubscribe channel disconnected — reconnecting");
                    break;
                }
            };

            let log_resp = msg.value;
            if log_resp.err.is_some() {
                continue;
            }
            let is_create_pool = log_resp
                .logs
                .iter()
                .any(|l| l.contains("CreatePool") || l.contains("Instruction: CreatePool"));
            if !is_create_pool {
                continue;
            }

            reap_finished(&mut children);
            if daily_state.entries_today >= args.max_daily_new_pool_trades {
                tracing::warn!(signature = %log_resp.signature, "daily new-pool entry cap reached — skipping candidate");
                continue;
            }
            if children.len() >= args.max_concurrent {
                tracing::debug!(signature = %log_resp.signature, "max concurrent discovery children reached — skipping candidate");
                continue;
            }

            let (pool_id, mint_a, mint_b) =
                match resolve_candidate_from_signature(&rpc, &log_resp.signature, &program_id) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(signature = %log_resp.signature, error = %e, "candidate resolution failed — skipping");
                        continue;
                    }
                };
            if !seen.insert(pool_id) {
                continue;
            }

            tracing::info!(pool = %pool_id, mint_a = %mint_a, mint_b = %mint_b, "candidate pool detected");

            let mut rejected = false;
            for mint in [mint_a, mint_b] {
                if trusted.contains(&mint) {
                    continue;
                }
                match onchain_risk::fetch_mint_authority_risk(&rpc, &mint) {
                    Ok(risk) if risk.is_risky() => {
                        tracing::info!(pool = %pool_id, mint = %mint, "pre-filter: mint/freeze authority present — rejected");
                        rejected = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(pool = %pool_id, mint = %mint, error = %e, "pre-filter: mint authority check failed — rejected (fail-closed)");
                        rejected = true;
                        break;
                    }
                }
            }
            if rejected {
                continue;
            }

            match account_resolver::fetch_pool_state(&rpc, &pool_id) {
                Ok(pool) => match onchain_risk::fetch_vault_liquidity(&rpc, &pool.token_vault_0) {
                    Ok(lamports) => {
                        let sol = lamports as f64 / 1_000_000_000.0;
                        if sol < args.min_liquidity_sol {
                            tracing::info!(pool = %pool_id, liquidity_sol = sol, "pre-filter: liquidity below minimum — rejected");
                            continue;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(pool = %pool_id, error = %e, "pre-filter: liquidity fetch failed — rejected (fail-closed)");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::debug!(pool = %pool_id, error = %e, "pre-filter: pool state fetch/parse failed — rejected (fail-closed)");
                    continue;
                }
            }

            tracing::info!(pool = %pool_id, "candidate passed pre-filter — spawning sniper child");
            match spawn_child(&args, &pool_id, &mint_a, &mint_b) {
                Ok(child) => {
                    children.push((pool_id, child));
                    daily_state.entries_today += 1;
                    save_daily_state(&daily_state_path, &daily_state);
                }
                Err(e) => {
                    tracing::warn!(pool = %pool_id, error = %e, "failed to spawn sniper child");
                }
            }
        }
    }
}
