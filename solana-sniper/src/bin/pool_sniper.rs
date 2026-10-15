//! pool-sniper — new Raydium CLMM pool discovery supervisor.
//!
//! Listens to the CLMM program's logs (`logsSubscribe`), picks up every new
//! `CreatePool`, keeps wSOL-paired pools on a watch list and re-checks them
//! (classic SPL mint, mint + freeze authority revoked, SOL-side liquidity
//! above the floor). A qualifying pool is traded by ONE `solana-sniper
//! --single-trade` process at a time that shares this supervisor's data
//! directory, so the bot's daily trade / loss limits and its single open
//! position apply across all pools. The child applies every regular gate
//! again (holder concentration, blocklist, risk manager, slippage) and
//! manages the exit itself.
//!
//! Mode is `paper` by default. `live` needs `--confirm-live` here AND the
//! regular live configuration (HSM, expected signer) in the environment.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::{
    RpcTransactionConfig, RpcTransactionLogsConfig, RpcTransactionLogsFilter,
};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sniper::amm::account_resolver;
use solana_sniper::onchain_risk;
use solana_sniper::pool_discovery::{
    find_create_pools, logs_contain_create_pool, resolve_indexes, token_side, Candidate,
    CheckOutcome, DiscoveryState, NewPool, PendingSignatures, RawInstruction, WatchList,
};
use solana_sniper::position::{LivePosition, PendingEntry};
use solana_sniper::redact::RedactingMakeWriter;
use solana_sniper::risk;
use solana_transaction_status::{option_serializer::OptionSerializer, UiInstruction};
use tracing_subscriber::EnvFilter;

const WSOL: &str = "So11111111111111111111111111111111111111112";
const CLMM_MAINNET: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
const CLMM_DEVNET: &str = "DRayAUgENGQBKVaX8owNhgzkEDyoHTGVEGHVJT1E9pfH";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    Paper,
    DryRun,
    Live,
}

impl Mode {
    fn flag(self) -> &'static str {
        match self {
            Mode::Paper => "--paper",
            Mode::DryRun => "--dry-run",
            Mode::Live => "--live",
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "New Raydium CLMM pool discovery supervisor for solana-sniper")]
struct Args {
    /// Trading mode of the spawned solana-sniper processes.
    #[arg(
        long,
        env = "SNIPER_DISCOVERY_MODE",
        value_enum,
        default_value = "paper"
    )]
    mode: Mode,

    /// Required (together with --mode live) to trade real funds.
    #[arg(long, env = "SNIPER_DISCOVERY_CONFIRM_LIVE", default_value_t = false)]
    confirm_live: bool,

    #[arg(
        long,
        env = "SNIPER_RPC",
        default_value = "https://api.mainnet-beta.solana.com"
    )]
    rpc: String,

    #[arg(
        long,
        env = "SNIPER_WS",
        default_value = "wss://api.mainnet-beta.solana.com"
    )]
    ws: String,

    /// Shared with the spawned processes (risk state, live position).
    #[arg(long, env = "SNIPER_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// solana-sniper binary (default: next to this executable).
    #[arg(long, env = "SNIPER_DISCOVERY_SNIPER_BIN")]
    sniper_bin: Option<PathBuf>,

    /// Minimum SOL-side liquidity before a pool is traded (SOL).
    #[arg(long, env = "SNIPER_MIN_LIQUIDITY_SOL", default_value_t = 1000.0)]
    min_liquidity_sol: f64,

    /// Pre-filter liquidity for a watched pool to be handed to a trading
    /// process (SOL). Defaults to --min-liquidity-sol (the entry threshold,
    /// which the trading process enforces again on its own).
    #[arg(long, env = "SNIPER_DISCOVERY_MIN_LIQUIDITY_SOL")]
    discovery_min_liquidity_sol: Option<f64>,

    /// How long a new pool stays on the watch list (seconds).
    #[arg(long, env = "SNIPER_DISCOVERY_WATCH_SECS", default_value_t = 1800)]
    watch_secs: u64,

    /// Interval between checks of one watched pool (seconds).
    #[arg(long, env = "SNIPER_DISCOVERY_RECHECK_SECS", default_value_t = 30)]
    recheck_secs: u64,

    /// Maximum pools on the watch list (oldest dropped first).
    #[arg(long, env = "SNIPER_DISCOVERY_MAX_WATCH", default_value_t = 50)]
    max_watch: usize,

    /// How long a spawned process may wait for its entry signal (seconds).
    #[arg(
        long,
        env = "SNIPER_DISCOVERY_ENTRY_WINDOW_SECS",
        default_value_t = 300
    )]
    entry_window_secs: u64,

    /// Sell an open position after this many seconds (passed to the child).
    #[arg(long, env = "SNIPER_DISCOVERY_MAX_HOLD_SECS", default_value_t = 3600)]
    max_hold_secs: u64,

    /// Maximum pools handed to a trading process per UTC day.
    #[arg(long, env = "SNIPER_DISCOVERY_MAX_POOLS_PER_DAY", default_value_t = 20)]
    max_pools_per_day: u32,

    /// Health / status endpoint of the supervisor ("" disables). The
    /// trading process's own metrics server only exists while it runs.
    #[arg(
        long,
        env = "SNIPER_DISCOVERY_HEALTH_ADDR",
        default_value = "127.0.0.1:9091"
    )]
    health_addr: String,
}

/// Supervisor liveness, shared with the health server.
#[derive(Default)]
struct Status {
    /// Unix ms of the last supervisor loop pass.
    loop_ms: AtomicU64,
    /// Unix ms of the last message on the CLMM log stream.
    ws_ms: AtomicU64,
    watched: AtomicU64,
    started_today: AtomicU64,
    child_running: AtomicBool,
    // Cumulative discovery counters (bounded set, no per-pool labels).
    create_pool_seen: AtomicU64,
    stream_reconnects: AtomicU64,
    signatures_evicted: AtomicU64,
    signatures_unresolved: AtomicU64,
    pools_watched: AtomicU64,
    pools_not_wsol: AtomicU64,
    pools_rejected: AtomicU64,
    pools_expired: AtomicU64,
    children_started: AtomicU64,
    child_startup_failures: AtomicU64,
    /// Unix ms of the last moment discovery may have missed a pool (stream
    /// reconnect, queue overflow, unresolvable CreatePool); 0 = never.
    last_gap_ms: AtomicU64,
    /// Discovery state could not be persisted: no new pools are started.
    state_persist_failed: AtomicBool,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Count a possible discovery gap and remember when it happened.
fn gap(st: &Status, c: &AtomicU64) {
    bump(c);
    st.last_gap_ms.store(now_ms(), Ordering::SeqCst);
}

/// /ready stays 503 this long after a possible gap: `logsSubscribe` has no
/// replay and a CLMM-wide signature backfill is too costly, so a gap is
/// reported instead of silently treated as complete coverage.
const GAP_READY_WINDOW_MS: u64 = 300_000;

/// Bound of the listener → supervisor signature channel.
const SIGNATURE_CHANNEL_CAP: usize = 1_024;

fn ready_verdict(st: &Status, now: u64) -> Result<(), String> {
    health_verdict(
        st.loop_ms.load(Ordering::SeqCst),
        st.ws_ms.load(Ordering::SeqCst),
        now,
    )?;
    if st.state_persist_failed.load(Ordering::SeqCst) {
        return Err("discovery state not persisted — new pools blocked".into());
    }
    let g = st.last_gap_ms.load(Ordering::SeqCst);
    if g != 0 && now.saturating_sub(g) < GAP_READY_WINDOW_MS {
        return Err(format!(
            "possible discovery gap {} s ago (reconnect / overflow / unresolved)",
            now.saturating_sub(g) / 1000
        ));
    }
    Ok(())
}

/// A busy program like the CLMM emits logs every few seconds; minutes of
/// silence mean the stream is dead even if the socket looks open.
const WS_SILENCE_LIMIT_MS: u64 = 180_000;
const LOOP_STALL_LIMIT_MS: u64 = 30_000;

fn health_verdict(loop_ms: u64, ws_ms: u64, now: u64) -> Result<(), String> {
    if now.saturating_sub(loop_ms) > LOOP_STALL_LIMIT_MS {
        return Err("supervisor loop stalled".into());
    }
    if ws_ms == 0 || now.saturating_sub(ws_ms) > WS_SILENCE_LIMIT_MS {
        return Err("no CLMM log stream messages (WebSocket down?)".into());
    }
    Ok(())
}

fn render_status(st: &Status, now: u64) -> String {
    let ws = st.ws_ms.load(Ordering::SeqCst);
    let g = st.last_gap_ms.load(Ordering::SeqCst);
    let mut out = format!(
        "pool_sniper_ws_last_message_age_ms {}\npool_sniper_last_gap_age_ms {}\npool_sniper_ready {}\npool_sniper_watch_list {}\npool_sniper_pools_started_today {}\npool_sniper_child_running {}\n",
        if ws == 0 { -1 } else { now.saturating_sub(ws) as i64 },
        if g == 0 { -1 } else { now.saturating_sub(g) as i64 },
        ready_verdict(st, now).is_ok() as u8,
        st.watched.load(Ordering::SeqCst),
        st.started_today.load(Ordering::SeqCst),
        st.child_running.load(Ordering::SeqCst) as u8,
    );
    for (name, c) in [
        ("create_pool_seen_total", &st.create_pool_seen),
        ("stream_reconnects_total", &st.stream_reconnects),
        ("signatures_evicted_total", &st.signatures_evicted),
        ("signatures_unresolved_total", &st.signatures_unresolved),
        ("pools_watched_total", &st.pools_watched),
        ("pools_not_wsol_total", &st.pools_not_wsol),
        ("pools_rejected_total", &st.pools_rejected),
        ("pools_expired_total", &st.pools_expired),
        ("children_started_total", &st.children_started),
        ("child_startup_failures_total", &st.child_startup_failures),
    ] {
        out.push_str(&format!(
            "pool_sniper_{name} {}\n",
            c.load(Ordering::Relaxed)
        ));
    }
    out
}

fn spawn_health_server(addr: &str, st: Arc<Status>) -> Result<(), String> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|e| format!("health server bind failed on {addr}: {e}"))?;
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let now = now_ms();
            let (code, body) = match path {
                "/health" => match health_verdict(
                    st.loop_ms.load(Ordering::SeqCst),
                    st.ws_ms.load(Ordering::SeqCst),
                    now,
                ) {
                    Ok(()) => ("200 OK", "ok".to_string()),
                    Err(e) => ("503 Service Unavailable", e),
                },
                "/ready" => match ready_verdict(&st, now) {
                    Ok(()) => ("200 OK", "ready".to_string()),
                    Err(e) => ("503 Service Unavailable", e),
                },
                "/metrics" => ("200 OK", render_status(&st, now)),
                _ => ("404 Not Found", "not found".to_string()),
            };
            let _ = write!(
                sock,
                "HTTP/1.1 {code}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Ok(())
}

fn save_discovery_state(dir: &Path, st: &DiscoveryState) -> Result<(), String> {
    let body = serde_json::to_vec(st).map_err(|e| e.to_string())?;
    solana_sniper::position::write_atomic(
        &dir.join(DiscoveryState::FILE),
        &dir.join(format!("{}.tmp", DiscoveryState::FILE)),
        &body,
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn clmm_program(rpc: &str) -> Pubkey {
    Pubkey::from_str(if rpc.contains("devnet") {
        CLMM_DEVNET
    } else {
        CLMM_MAINNET
    })
    .unwrap()
}

fn validate(args: &Args) -> Result<(), String> {
    if args.mode == Mode::Live && !args.confirm_live {
        return Err(
            "--mode live requires --confirm-live (SNIPER_DISCOVERY_CONFIRM_LIVE=true)".into(),
        );
    }
    if let Some(d) = args.discovery_min_liquidity_sol {
        if !d.is_finite() || d < 1.0 {
            return Err("--discovery-min-liquidity-sol must be >= 1".into());
        }
    }
    if !args.min_liquidity_sol.is_finite() || args.min_liquidity_sol < 20.0 {
        return Err("--min-liquidity-sol must be >= 20".into());
    }
    if args.recheck_secs == 0 || args.watch_secs == 0 || args.max_pools_per_day == 0 {
        return Err("--recheck-secs, --watch-secs and --max-pools-per-day must be > 0".into());
    }
    Ok(())
}

/// Child process environment overrides: every per-run setting the env file
/// may also carry is removed and set explicitly by `child_args`.
const CHILD_ENV_REMOVE: &[&str] = &[
    "SNIPER_PAPER",
    "SNIPER_DRY_RUN",
    "SNIPER_LIVE",
    "SNIPER_CONFIRM_LIVE",
    "SNIPER_POOL_ID",
    "SNIPER_INPUT_MINT",
    "SNIPER_OUTPUT_MINT",
    "SNIPER_ITERATIONS",
    "SNIPER_DATA_DIR",
    "SNIPER_SINGLE_TRADE",
    "SNIPER_ENTRY_WINDOW_SECS",
    "SNIPER_MIN_LIQUIDITY_SOL",
    "SNIPER_MAX_HOLD_SECS",
    "SNIPER_LIVE_RISK_DATA",
];

fn child_args(args: &Args, pool: &str, input_mint: &str, output_mint: &str) -> Vec<String> {
    let mut v: Vec<String> = vec![args.mode.flag().into()];
    if args.mode == Mode::Live {
        v.push("--confirm-live".into());
    }
    for (k, val) in [
        ("--pool-id", pool.to_string()),
        ("--input-mint", input_mint.to_string()),
        ("--output-mint", output_mint.to_string()),
        ("--iterations", "0".into()),
        ("--data-dir", args.data_dir.display().to_string()),
        ("--entry-window-secs", args.entry_window_secs.to_string()),
        ("--min-liquidity-sol", args.min_liquidity_sol.to_string()),
        ("--max-hold-secs", args.max_hold_secs.to_string()),
    ] {
        v.push(k.into());
        v.push(val);
    }
    v.push("--single-trade".into());
    v.push("--live-risk-data".into());
    v
}

struct Running {
    child: Child,
    pool: String,
    started: Instant,
    term_sent: Option<Instant>,
}

fn spawn_child(
    args: &Args,
    bin: &Path,
    pool: &str,
    input_mint: &str,
    output_mint: &str,
) -> Result<Running, String> {
    let mut cmd = Command::new(bin);
    for k in CHILD_ENV_REMOVE {
        cmd.env_remove(k);
    }
    if args.mode != Mode::DryRun {
        // Only valid with --dry-run; the child refuses to start otherwise.
        cmd.env_remove("SNIPER_SIMULATE_PAYER");
    }
    cmd.args(child_args(args, pool, input_mint, output_mint));
    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    Ok(Running {
        child,
        pool: pool.to_string(),
        started: Instant::now(),
        term_sent: None,
    })
}

fn send_sigterm(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

/// Background thread: CLMM program logs → CreatePool signatures. Reconnects
/// with backoff; a silent socket (no message for 120 s on a program this
/// busy) counts as dead.
fn spawn_log_listener(
    ws: String,
    program: Pubkey,
    tx: mpsc::SyncSender<String>,
    shutdown: Arc<AtomicBool>,
    status: Arc<Status>,
) {
    std::thread::spawn(move || {
        let mut backoff = 1u64;
        while !shutdown.load(Ordering::SeqCst) {
            let sub = solana_client::pubsub_client::PubsubClient::logs_subscribe(
                &ws,
                RpcTransactionLogsFilter::Mentions(vec![program.to_string()]),
                RpcTransactionLogsConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                },
            );
            let (mut client, receiver) = match sub {
                Ok(v) => v,
                Err(e) => {
                    gap(&status, &status.stream_reconnects);
                    tracing::warn!(target: "discovery", error = %e, backoff, "logsSubscribe failed — retrying");
                    std::thread::sleep(Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };
            tracing::info!(target: "discovery", program = %program, "subscribed to CLMM program logs");
            backoff = 1;
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match receiver.recv_timeout(Duration::from_secs(120)) {
                    Ok(resp) => {
                        status.ws_ms.store(now_ms(), Ordering::SeqCst);
                        let v = resp.value;
                        if v.err.is_none() && logs_contain_create_pool(&v.logs) {
                            bump(&status.create_pool_seen);
                            tracing::info!(target: "discovery", signature = %v.signature, "CreatePool seen");
                            match tx.try_send(v.signature) {
                                Ok(()) => {}
                                Err(mpsc::TrySendError::Full(_)) => {
                                    gap(&status, &status.signatures_evicted);
                                    tracing::warn!(target: "discovery", "signature channel full — CreatePool dropped");
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => return,
                            }
                        }
                    }
                    Err(e) => {
                        gap(&status, &status.stream_reconnects);
                        tracing::warn!(target: "discovery", error = %e, "log stream ended — reconnecting");
                        break;
                    }
                }
            }
            let _ = client.send_unsubscribe();
            let _ = client.shutdown();
        }
    });
}

/// Fetch a CreatePool transaction and extract the new pools (top-level and
/// CPI). `Ok(None)` = not served by the RPC yet.
fn resolve_signature(
    rpc: &RpcClient,
    program: &Pubkey,
    sig: &str,
) -> Result<Option<Vec<NewPool>>, String> {
    let signature = Signature::from_str(sig).map_err(|e| format!("bad signature: {e}"))?;
    let cfg = RpcTransactionConfig {
        encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let tx = match rpc.get_transaction_with_config(&signature, cfg) {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            // A just-confirmed transaction is often not served yet.
            if msg.contains("invalid type: null") || msg.contains("not found") {
                return Ok(None);
            }
            return Err(msg);
        }
    };
    let vtx = tx
        .transaction
        .transaction
        .decode()
        .ok_or("undecodable transaction")?;
    let meta = tx.transaction.meta.ok_or("transaction without meta")?;
    let mut keys: Vec<Pubkey> = vtx.message.static_account_keys().to_vec();
    if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
        for k in loaded.writable.iter().chain(loaded.readonly.iter()) {
            keys.push(Pubkey::from_str(k).map_err(|e| format!("bad loaded address: {e}"))?);
        }
    }
    let mut owned: Vec<(Pubkey, Vec<Pubkey>, Vec<u8>)> = Vec::new();
    for ix in vtx.message.instructions() {
        let (Some(pid), Some(accs)) = (
            keys.get(ix.program_id_index as usize),
            resolve_indexes(&keys, &ix.accounts),
        ) else {
            continue;
        };
        owned.push((*pid, accs, ix.data.clone()));
    }
    if let OptionSerializer::Some(inner) = &meta.inner_instructions {
        for group in inner {
            for ix in &group.instructions {
                let UiInstruction::Compiled(c) = ix else {
                    continue;
                };
                let (Some(pid), Some(accs), Ok(data)) = (
                    keys.get(c.program_id_index as usize),
                    resolve_indexes(&keys, &c.accounts),
                    solana_sdk::bs58::decode(&c.data).into_vec(),
                ) else {
                    continue;
                };
                owned.push((*pid, accs, data));
            }
        }
    }
    let raw: Vec<RawInstruction> = owned
        .iter()
        .map(|(p, a, d)| RawInstruction {
            program_id: *p,
            accounts: a.clone(),
            data: d,
        })
        .collect();
    Ok(Some(find_create_pools(program, &raw)))
}

/// Pre-filter one watched pool (cheap; the child re-checks everything).
fn check_candidate(
    rpc: &RpcClient,
    program: &Pubkey,
    c: &mut Candidate,
    min_liq_lamports: u64,
) -> CheckOutcome {
    if !c.mint_ok {
        match onchain_risk::fetch_mint_authority_risk(rpc, &c.token_mint) {
            Ok(r) if r.is_risky() => {
                return CheckOutcome::Reject(format!(
                    "mint authority present: {} / freeze authority present: {}",
                    r.mint_authority_present, r.freeze_authority_present
                ))
            }
            Ok(_) => c.mint_ok = true,
            Err(e) if e.contains("Token-2022") => return CheckOutcome::Reject(e),
            Err(e) => return CheckOutcome::Retry(e),
        }
    }
    let pool = match account_resolver::fetch_pool_state(rpc, &c.pool.pool_id, program) {
        Ok(p) => p,
        Err(e) => return CheckOutcome::Retry(e),
    };
    let wsol = Pubkey::from_str(WSOL).unwrap();
    let sol_vault = if pool.token_mint_0 == wsol && pool.token_mint_1 == c.token_mint {
        pool.token_vault_0
    } else if pool.token_mint_1 == wsol && pool.token_mint_0 == c.token_mint {
        pool.token_vault_1
    } else {
        return CheckOutcome::Reject("pool state mints do not match CreatePool".into());
    };
    if pool.liquidity == 0 {
        return CheckOutcome::Retry("no in-range liquidity yet".into());
    }
    match onchain_risk::fetch_vault_liquidity(rpc, &sol_vault) {
        Ok(l) if l >= min_liq_lamports => CheckOutcome::Ready,
        Ok(l) => CheckOutcome::Retry(format!(
            "SOL liquidity {:.2} < {:.2}",
            l as f64 / 1e9,
            min_liq_lamports as f64 / 1e9
        )),
        Err(e) => CheckOutcome::Retry(e),
    }
}

/// Whether the shared risk state still allows a new entry. Live only: paper
/// and dry-run never write trades into the risk state.
fn risk_allows_entry(args: &Args) -> Result<(), String> {
    if args.mode != Mode::Live {
        return Ok(());
    }
    let cfg = risk::RiskConfig::production_defaults(args.data_dir.clone())?;
    let rm = risk::RiskManager::new(cfg);
    let size = solana_sniper::strategy::StrategyConfig::default().max_trade_size_lamports;
    rm.pre_trade_check(size, 50).map_err(|e| e.to_string())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,solana_sniper=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .json()
        .with_writer(RedactingMakeWriter::new(std::io::stdout))
        .init();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    init_tracing();
    validate(&args)?;
    std::fs::create_dir_all(&args.data_dir)?;

    let sniper_bin = match &args.sniper_bin {
        Some(p) => p.clone(),
        None => std::env::current_exe()?
            .parent()
            .ok_or("cannot locate executable directory")?
            .join("solana-sniper"),
    };
    if !sniper_bin.exists() {
        return Err(format!("solana-sniper binary not found at {}", sniper_bin.display()).into());
    }
    let program = clmm_program(&args.rpc);
    let wsol = Pubkey::from_str(WSOL).unwrap();
    let min_liq = (args
        .discovery_min_liquidity_sol
        .unwrap_or(args.min_liquidity_sol)
        * 1e9) as u64;
    tracing::info!(
        target: "discovery",
        mode = ?args.mode,
        rpc = %args.rpc,
        program = %program,
        min_liquidity_sol = args.min_liquidity_sol,
        watch_secs = args.watch_secs,
        max_pools_per_day = args.max_pools_per_day,
        "pool-sniper starting"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = shutdown.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("signal runtime");
            rt.block_on(async {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            });
            flag.store(true, Ordering::SeqCst);
        });
    }

    let rpc = RpcClient::new_with_commitment(args.rpc.clone(), CommitmentConfig::confirmed());
    let (sig_tx, sig_rx) = mpsc::sync_channel::<String>(SIGNATURE_CHANNEL_CAP);
    let status = Arc::new(Status::default());
    status.loop_ms.store(now_ms(), Ordering::SeqCst);
    if !args.health_addr.trim().is_empty() {
        spawn_health_server(args.health_addr.trim(), status.clone())?;
        tracing::info!(target: "discovery", addr = %args.health_addr, "health server listening (/health, /ready, /metrics)");
    }
    spawn_log_listener(
        args.ws.clone(),
        program,
        sig_tx,
        shutdown.clone(),
        status.clone(),
    );

    let mut pending = PendingSignatures::default();
    let mut watch = WatchList::new(
        args.max_watch,
        args.watch_secs * 1000,
        args.recheck_secs * 1000,
    );
    // Restart-safe discovery state; a corrupt file refuses to start (it
    // would silently reset the daily pool cap).
    let state_path = args.data_dir.join(DiscoveryState::FILE);
    let mut disc_state = match std::fs::read_to_string(&state_path) {
        Ok(raw) => DiscoveryState::parse(&raw)
            .map_err(|e| format!("{e} ({}) — fix or remove it", state_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiscoveryState::default(),
        Err(e) => return Err(format!("cannot read {}: {e}", state_path.display()).into()),
    };
    for p in &disc_state.traded_pools {
        if let Ok(pk) = Pubkey::from_str(p) {
            watch.mark_seen(pk);
        }
    }
    tracing::info!(target: "discovery", traded_pools = disc_state.traded_pools.len(), started_today = disc_state.daily.count(now_ms()), "discovery state restored");
    let mut running: Option<Running> = None;
    let mut risk_blocked_until: Option<Instant> = None;
    // A process that runs longer than this is stopped (SIGTERM; a live
    // position is persisted and resumed by the next process).
    let max_child = Duration::from_secs(args.entry_window_secs + args.max_hold_secs + 900);

    loop {
        let now = now_ms();
        status.loop_ms.store(now, Ordering::SeqCst);
        status.watched.store(watch.len() as u64, Ordering::SeqCst);
        status
            .started_today
            .store(disc_state.daily.count(now) as u64, Ordering::SeqCst);
        status
            .child_running
            .store(running.is_some(), Ordering::SeqCst);
        // 1) Child lifecycle.
        if let Some(r) = running.as_mut() {
            match r.child.try_wait() {
                Ok(Some(exit)) => {
                    let secs = r.started.elapsed().as_secs();
                    if !exit.success() && secs < 15 {
                        // Fails right at startup: a configuration error, not
                        // a market condition. Back off instead of burning
                        // through the pools.
                        bump(&status.child_startup_failures);
                        tracing::error!(target: "discovery", pool = %r.pool, status = %exit, "trading process failed at startup — check its log above; pausing 60 s");
                        risk_blocked_until = Some(Instant::now() + Duration::from_secs(60));
                    } else {
                        tracing::info!(target: "discovery", pool = %r.pool, status = %exit, secs, "trading process finished");
                    }
                    running = None;
                }
                Ok(None) => {
                    if shutdown.load(Ordering::SeqCst) || r.started.elapsed() > max_child {
                        match r.term_sent {
                            None => {
                                tracing::warn!(target: "discovery", pool = %r.pool, "stopping trading process (SIGTERM)");
                                send_sigterm(r.child.id());
                                r.term_sent = Some(Instant::now());
                            }
                            Some(t) if t.elapsed() > Duration::from_secs(120) => {
                                tracing::error!(target: "discovery", pool = %r.pool, "trading process ignored SIGTERM — killing");
                                let _ = r.child.kill();
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(target: "discovery", error = %e, "cannot poll trading process");
                }
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            if running.is_none() {
                tracing::info!(target: "discovery", "shutdown complete");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        // 2) New CreatePool signatures → watch list.
        while let Ok(sig) = sig_rx.try_recv() {
            if pending.push(sig) {
                gap(&status, &status.signatures_evicted);
                tracing::warn!(target: "discovery", "pending signature queue full — oldest CreatePool dropped");
            }
        }
        for _ in 0..pending.len().min(5) {
            let Some(sig) = pending.pop() else { break };
            match resolve_signature(&rpc, &program, &sig) {
                Ok(Some(pools)) => {
                    pending.done(&sig);
                    for p in pools {
                        match token_side(&p, &wsol) {
                            Some(token) => {
                                if watch.add(p, token, now_ms()) {
                                    bump(&status.pools_watched);
                                    tracing::info!(target: "discovery", pool = %p.pool_id, token = %token, watched = watch.len(), "new wSOL pool on watch list");
                                }
                            }
                            None => {
                                bump(&status.pools_not_wsol);
                                tracing::debug!(target: "discovery", pool = %p.pool_id, "not paired with wSOL — skipped")
                            }
                        }
                    }
                }
                Ok(None) => {
                    if !pending.retry(sig) {
                        gap(&status, &status.signatures_unresolved);
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "discovery", signature = %sig, error = %e, "CreatePool transaction fetch failed");
                    if !pending.retry(sig) {
                        gap(&status, &status.signatures_unresolved);
                    }
                }
            }
        }
        let now = now_ms();
        for c in watch.expire(now) {
            bump(&status.pools_expired);
            tracing::info!(target: "discovery", pool = %c.pool.pool_id, checks = c.checks, last = %c.last_reason, "watch time over — dropped");
        }

        // 3) Start a trading process when idle.
        if running.is_none() {
            // An open live position always comes first (shared data dir) —
            // also one whose entry is still unreconciled (pending_entry.json:
            // the trading process died while confirming it).
            let open = LivePosition::load(&args.data_dir).and_then(|p| match p {
                Some(p) => Ok(Some(p)),
                None => PendingEntry::load(&args.data_dir).map(|e| e.map(|e| e.position)),
            });
            match open {
                Ok(Some(pos)) if args.mode == Mode::Live => {
                    tracing::warn!(target: "discovery", pool = %pos.pool_id, "open live position — resuming its management");
                    if let Ok(pk) = Pubkey::from_str(&pos.pool_id) {
                        watch.mark_seen(pk);
                    }
                    match spawn_child(
                        &args,
                        &sniper_bin,
                        &pos.pool_id,
                        &pos.input_mint,
                        &pos.output_mint,
                    ) {
                        Ok(r) => running = Some(r),
                        Err(e) => {
                            tracing::error!(target: "discovery", error = %e, "cannot resume live position");
                            std::thread::sleep(Duration::from_secs(10));
                        }
                    }
                    continue;
                }
                Err(e) if args.mode == Mode::Live => {
                    return Err(
                        format!("live position record unreadable (fail-closed): {e}").into(),
                    );
                }
                _ => {}
            }

            let blocked = risk_blocked_until.is_some_and(|t| Instant::now() < t)
                || status.state_persist_failed.load(Ordering::SeqCst);
            if !blocked && !disc_state.daily.reached(now, args.max_pools_per_day) {
                match risk_allows_entry(&args) {
                    Err(e) => {
                        tracing::warn!(target: "discovery", reason = %e, "risk limits reached — no new pools for now");
                        risk_blocked_until = Some(Instant::now() + Duration::from_secs(300));
                    }
                    Ok(()) => {
                        risk_blocked_until = None;
                        // At most a few checks per loop to stay within RPC limits.
                        for _ in 0..3 {
                            let Some(mut c) = watch.take_due(now_ms()) else {
                                break;
                            };
                            match check_candidate(&rpc, &program, &mut c, min_liq) {
                                CheckOutcome::Ready => {
                                    let pool = c.pool.pool_id.to_string();
                                    tracing::info!(target: "discovery", pool = %pool, token = %c.token_mint, checks = c.checks, "pool qualifies — starting trading process");
                                    match spawn_child(
                                        &args,
                                        &sniper_bin,
                                        &pool,
                                        WSOL,
                                        &c.token_mint.to_string(),
                                    ) {
                                        Ok(r) => {
                                            disc_state.record_start(&pool, now_ms());
                                            if let Err(e) =
                                                save_discovery_state(&args.data_dir, &disc_state)
                                            {
                                                status
                                                    .state_persist_failed
                                                    .store(true, Ordering::SeqCst);
                                                tracing::error!(target: "discovery", error = %e, "cannot persist discovery state — no further pools will be started");
                                            }
                                            bump(&status.children_started);
                                            running = Some(r);
                                        }
                                        Err(e) => {
                                            tracing::error!(target: "discovery", error = %e, "cannot start trading process");
                                        }
                                    }
                                    break;
                                }
                                CheckOutcome::Retry(reason) => {
                                    tracing::debug!(target: "discovery", pool = %c.pool.pool_id, reason = %reason, "not ready — will re-check");
                                    watch.reschedule(c, now_ms(), reason);
                                }
                                CheckOutcome::Reject(reason) => {
                                    bump(&status.pools_rejected);
                                    tracing::info!(target: "discovery", pool = %c.pool.pool_id, reason = %reason, "pool rejected");
                                }
                            }
                        }
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let mut v = vec!["pool-sniper"];
        v.extend_from_slice(extra);
        Args::try_parse_from(v).unwrap()
    }

    #[test]
    fn defaults_to_paper_and_live_needs_confirmation() {
        let a = args(&[]);
        assert_eq!(a.mode, Mode::Paper);
        assert!(validate(&a).is_ok());
        assert!(validate(&args(&["--mode", "live"])).is_err());
        assert!(validate(&args(&["--mode", "live", "--confirm-live"])).is_ok());
        assert!(validate(&args(&["--min-liquidity-sol", "5"])).is_err());
    }

    #[test]
    fn child_args_are_explicit_and_single_trade() {
        let a = args(&["--mode", "dry-run", "--data-dir", "/var/lib/x"]);
        let v = child_args(&a, "POOL", WSOL, "TOKEN");
        assert_eq!(v[0], "--dry-run");
        assert!(!v.contains(&"--confirm-live".to_string()));
        assert!(v.contains(&"--single-trade".to_string()));
        assert!(v.contains(&"--live-risk-data".to_string()));
        let pos = |k: &str| v.iter().position(|x| x == k).unwrap();
        assert_eq!(v[pos("--pool-id") + 1], "POOL");
        assert_eq!(v[pos("--input-mint") + 1], WSOL);
        assert_eq!(v[pos("--output-mint") + 1], "TOKEN");
        assert_eq!(v[pos("--iterations") + 1], "0");
        assert_eq!(v[pos("--data-dir") + 1], "/var/lib/x");
        let live = args(&["--mode", "live", "--confirm-live"]);
        let v = child_args(&live, "P", WSOL, "T");
        assert_eq!(
            &v[..2],
            &["--live".to_string(), "--confirm-live".to_string()]
        );
    }

    #[test]
    fn child_env_overrides_cover_every_mode_switch() {
        for k in [
            "SNIPER_PAPER",
            "SNIPER_DRY_RUN",
            "SNIPER_LIVE",
            "SNIPER_CONFIRM_LIVE",
        ] {
            assert!(CHILD_ENV_REMOVE.contains(&k), "{k}");
        }
    }

    #[test]
    fn health_requires_live_loop_and_log_stream() {
        let now = 10_000_000;
        assert!(health_verdict(now, now - 1_000, now).is_ok());
        assert!(health_verdict(now, 0, now).is_err(), "never received a log");
        assert!(health_verdict(now, now - WS_SILENCE_LIMIT_MS - 1, now).is_err());
        assert!(health_verdict(now - LOOP_STALL_LIMIT_MS - 1, now, now).is_err());
        let st = Status::default();
        st.watched.store(3, Ordering::SeqCst);
        let out = render_status(&st, now);
        assert!(out.contains("pool_sniper_watch_list 3"));
        assert!(out.contains("pool_sniper_ws_last_message_age_ms -1"));
    }

    #[test]
    fn ready_reports_gaps_and_persist_failures() {
        let st = Status::default();
        let now = now_ms();
        st.loop_ms.store(now, Ordering::SeqCst);
        st.ws_ms.store(now, Ordering::SeqCst);
        assert!(ready_verdict(&st, now).is_ok());
        gap(&st, &st.stream_reconnects);
        assert!(ready_verdict(&st, now_ms()).is_err(), "fresh gap");
        // Once the window has passed (with a live loop and stream) it is ready.
        let later = now_ms() + GAP_READY_WINDOW_MS;
        st.loop_ms.store(later, Ordering::SeqCst);
        st.ws_ms.store(later, Ordering::SeqCst);
        assert!(ready_verdict(&st, later).is_ok());
        let st2 = Status::default();
        st2.loop_ms.store(now, Ordering::SeqCst);
        st2.ws_ms.store(now, Ordering::SeqCst);
        st2.state_persist_failed.store(true, Ordering::SeqCst);
        assert!(ready_verdict(&st2, now).is_err());
        assert!(render_status(&st2, now).contains("pool_sniper_ready 0"));
    }

    #[test]
    fn status_renders_bounded_counters() {
        let st = Status::default();
        bump(&st.create_pool_seen);
        bump(&st.signatures_evicted);
        let out = render_status(&st, 1);
        assert!(out.contains("pool_sniper_create_pool_seen_total 1"));
        assert!(out.contains("pool_sniper_signatures_evicted_total 1"));
        assert!(out.contains("pool_sniper_child_startup_failures_total 0"));
        assert!(validate(&args(&["--discovery-min-liquidity-sol", "0.5"])).is_err());
        assert!(validate(&args(&["--discovery-min-liquidity-sol", "5"])).is_ok());
    }

    #[test]
    fn program_id_by_cluster() {
        assert_eq!(
            clmm_program("https://api.devnet.solana.com").to_string(),
            CLMM_DEVNET
        );
        assert_eq!(
            clmm_program("https://mainnet.helius-rpc.com/?api-key=x").to_string(),
            CLMM_MAINNET
        );
    }
}
