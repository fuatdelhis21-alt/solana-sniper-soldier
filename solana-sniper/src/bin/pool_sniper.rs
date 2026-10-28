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
    find_create_pools, logs_contain_create_pool, parse_gecko_pools, resolve_indexes, token_side,
    BoundedSet, Candidate, CheckOutcome, DiscoveryState, NewPool, PendingSignatures,
    RawInstruction, WatchList, GECKO_CLMM_DEX_ID,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Source {
    Logs,
    Geckoterminal,
}

/// What the discovery sources hand the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DiscoveryEvent {
    /// A transaction whose logs show a CreatePool (logs source).
    Signature(String),
    /// A pool address named by GeckoTerminal (verified on-chain later).
    Pool(Pubkey),
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

    /// Where new pools come from. `logs` subscribes to every CLMM program
    /// log over WebSocket (fastest, but a heavy stream that exhausts free RPC
    /// plans); `geckoterminal` polls GeckoTerminal's new-pools listing and
    /// verifies every named pool on-chain (no WebSocket, ~1-2 min later).
    #[arg(
        long,
        env = "SNIPER_DISCOVERY_SOURCE",
        value_enum,
        default_value = "logs"
    )]
    discovery_source: Source,

    /// GeckoTerminal listing polled by `--discovery-source geckoterminal`;
    /// `{page}` is replaced by the page number.
    #[arg(
        long,
        env = "SNIPER_DISCOVERY_GECKO_URL",
        default_value = "https://api.geckoterminal.com/api/v2/networks/solana/new_pools?page={page}"
    )]
    gecko_url: String,

    /// Pages fetched per poll (20 pools each; the free API allows about
    /// 10 calls per minute).
    #[arg(long, env = "SNIPER_DISCOVERY_GECKO_PAGES", default_value_t = 3)]
    gecko_pages: u32,

    /// Seconds between GeckoTerminal polls (min 30).
    #[arg(long, env = "SNIPER_DISCOVERY_POLL_SECS", default_value_t = 60)]
    poll_secs: u64,

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

/// Bound of the queue of listed pools awaiting on-chain verification.
const LISTED_POOLS_CAP: usize = 256;
/// Waits before re-reading a listed pool after the n-th failed read (RPC
/// errors such as HTTP 429 are usually temporary). About 32 min in all,
/// the length of the default watch window; then the pool is dropped.
const LISTED_POOL_RETRY_MS: [u64; 4] = [30_000, 120_000, 600_000, 1_200_000];

/// Delay before the next read of a listed pool after `failures` failed
/// reads; `None` once the retries are used up.
fn listed_retry_delay_ms(failures: u8) -> Option<u64> {
    failures
        .checked_sub(1)
        .and_then(|i| LISTED_POOL_RETRY_MS.get(i as usize).copied())
}

/// A read failure that may succeed later (the RPC call itself failed:
/// rate limit, timeout, network), as opposed to an account that was read
/// and is not a CLMM pool, which is final.
fn transient_pool_read_error(e: &str) -> bool {
    e.starts_with("failed to fetch pool state")
}

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
        return Err(
            "no data from the discovery source (CLMM log stream / GeckoTerminal down?)".into(),
        );
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
    if args.discovery_source == Source::Geckoterminal {
        if !args.gecko_url.starts_with("https://") || !args.gecko_url.contains("{page}") {
            return Err("--gecko-url must be an https:// URL containing {page}".into());
        }
        if !(1..=10).contains(&args.gecko_pages) {
            return Err("--gecko-pages must be 1..=10".into());
        }
        if args.poll_secs < 30 {
            return Err("--poll-secs must be >= 30 (free API rate limit)".into());
        }
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
/// The stream counts as down for an alarm after this much silence; the
/// alarm repeats at the same interval while it lasts.
const STREAM_DOWN_ALARM_MS: u64 = 600_000;

/// Silence (ms) to report when an ERROR alarm is due: the stream has been
/// silent (or never delivered since `started_ms`) for at least
/// `STREAM_DOWN_ALARM_MS` and the previous alarm is at least as old.
fn silence_alarm_due(
    last_msg_ms: u64,
    started_ms: u64,
    last_alarm_ms: u64,
    now: u64,
) -> Option<u64> {
    let since = if last_msg_ms == 0 {
        started_ms
    } else {
        last_msg_ms
    };
    let silent = now.saturating_sub(since);
    (silent >= STREAM_DOWN_ALARM_MS && now.saturating_sub(last_alarm_ms) >= STREAM_DOWN_ALARM_MS)
        .then_some(silent)
}

/// An error with its whole source chain ("websocket error: HTTP error: 429
/// Too Many Requests"): the pubsub client's own message hides the cause.
/// Secrets in URLs are redacted.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    solana_sniper::redact::redact_secrets(&out)
}

fn spawn_log_listener(
    ws: String,
    program: Pubkey,
    tx: mpsc::SyncSender<DiscoveryEvent>,
    shutdown: Arc<AtomicBool>,
    status: Arc<Status>,
) {
    std::thread::spawn(move || {
        let mut backoff = 1u64;
        let started_ms = now_ms();
        let mut last_alarm_ms = 0u64;
        while !shutdown.load(Ordering::SeqCst) {
            if let Some(silent_ms) = silence_alarm_due(
                status.ws_ms.load(Ordering::SeqCst),
                started_ms,
                last_alarm_ms,
                now_ms(),
            ) {
                last_alarm_ms = now_ms();
                tracing::error!(
                    target: "discovery",
                    silent_minutes = silent_ms / 60_000,
                    reconnect_attempts = status.stream_reconnects.load(Ordering::Relaxed),
                    "CLMM log stream DOWN — no new pools are being discovered (check the RPC/WS provider: quota, plan, key)"
                );
            }
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
                    tracing::warn!(target: "discovery", error = %error_chain(&e), backoff, "logsSubscribe failed — retrying");
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
                            match tx.try_send(DiscoveryEvent::Signature(v.signature)) {
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

/// GeckoTerminal source: every `poll_secs`, fetch `pages` pages of the
/// new-pools listing and emit each Raydium CLMM pool created within the
/// watch window once. A poll whose oldest pool is newer than the previous
/// poll means pools beyond the fetched pages may have been missed: counted
/// as a gap (/ready 503 for a while). Bounded: fixed pages per poll, one
/// HTTP request per page, a bounded channel and a bounded dedup set.
#[allow(clippy::too_many_arguments)]
fn spawn_gecko_poller(
    url: String,
    pages: u32,
    poll_secs: u64,
    max_age_ms: u64,
    tx: mpsc::SyncSender<DiscoveryEvent>,
    shutdown: Arc<AtomicBool>,
    status: Arc<Status>,
) {
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("solana-sniper-pool-discovery")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(target: "discovery", error = %e, "cannot build the GeckoTerminal client — discovery stopped");
                return;
            }
        };
        let started_ms = now_ms();
        let mut last_alarm_ms = 0u64;
        let mut last_poll_ms: Option<u64> = None;
        let mut emitted = BoundedSet::new(20_000);
        while !shutdown.load(Ordering::SeqCst) {
            if let Some(silent_ms) = silence_alarm_due(
                status.ws_ms.load(Ordering::SeqCst),
                started_ms,
                last_alarm_ms,
                now_ms(),
            ) {
                last_alarm_ms = now_ms();
                tracing::error!(
                    target: "discovery",
                    silent_minutes = silent_ms / 60_000,
                    "discovery source DOWN — GeckoTerminal polls failing, no new pools are being discovered"
                );
            }
            let poll_start = now_ms();
            let mut oldest: Option<u64> = None;
            let mut ok = true;
            for page in 1..=pages {
                let page_url = url.replace("{page}", &page.to_string());
                let fetch = || {
                    client
                        .get(&page_url)
                        .header("Accept", "application/json")
                        .send()
                        .and_then(|r| r.error_for_status())
                        .and_then(|r| r.text())
                        .map_err(|e| error_chain(&e))
                        .and_then(|b| {
                            parse_gecko_pools(&b, GECKO_CLMM_DEX_ID, now_ms(), max_age_ms)
                        })
                };
                // The free API answers an occasional 429; one retry a few
                // seconds later usually passes and avoids a needless gap.
                let body = match fetch() {
                    Err(e) if e.contains("429") && !shutdown.load(Ordering::SeqCst) => {
                        std::thread::sleep(Duration::from_secs(GECKO_RETRY_AFTER_429_SECS));
                        fetch()
                    }
                    other => other,
                };
                match body {
                    Ok(p) => {
                        if p.malformed > 0 {
                            tracing::debug!(target: "discovery", page, malformed = p.malformed, "GeckoTerminal entries skipped");
                        }
                        oldest = match (oldest, p.oldest_created_ms) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                        for pool in p.pools {
                            if !emitted.insert(pool) {
                                continue;
                            }
                            bump(&status.create_pool_seen);
                            tracing::info!(target: "discovery", pool = %pool, "new CLMM pool listed by GeckoTerminal (to verify on-chain)");
                            match tx.try_send(DiscoveryEvent::Pool(pool)) {
                                Ok(()) => {}
                                Err(mpsc::TrySendError::Full(_)) => {
                                    gap(&status, &status.signatures_evicted);
                                    tracing::warn!(target: "discovery", "discovery channel full — pool dropped");
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => return,
                            }
                        }
                    }
                    Err(e) => {
                        ok = false;
                        gap(&status, &status.stream_reconnects);
                        tracing::warn!(target: "discovery", page, error = %e, "GeckoTerminal poll failed — retrying next interval");
                        break;
                    }
                }
            }
            if ok {
                status.ws_ms.store(now_ms(), Ordering::SeqCst);
                if gecko_gap(last_poll_ms, oldest) {
                    gap(&status, &status.signatures_unresolved);
                    tracing::warn!(target: "discovery", pages, "all fetched pages are newer than the previous poll — pools may have been missed (raise SNIPER_DISCOVERY_GECKO_PAGES or lower SNIPER_DISCOVERY_POLL_SECS)");
                }
                last_poll_ms = Some(poll_start);
            }
            for _ in 0..poll_secs {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

/// Wait before the single retry of a GeckoTerminal page answered with 429.
const GECKO_RETRY_AFTER_429_SECS: u64 = 5;

/// True when the oldest pool of a poll was created after the previous poll
/// started: the pages did not reach back far enough, pools may be missing.
/// The first poll has nothing to compare with.
fn gecko_gap(last_poll_ms: Option<u64>, oldest_created_ms: Option<u64>) -> bool {
    matches!((last_poll_ms, oldest_created_ms), (Some(last), Some(oldest)) if oldest > last)
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
    let (sig_tx, sig_rx) = mpsc::sync_channel::<DiscoveryEvent>(SIGNATURE_CHANNEL_CAP);
    let status = Arc::new(Status::default());
    status.loop_ms.store(now_ms(), Ordering::SeqCst);
    if !args.health_addr.trim().is_empty() {
        spawn_health_server(args.health_addr.trim(), status.clone())?;
        tracing::info!(target: "discovery", addr = %args.health_addr, "health server listening (/health, /ready, /metrics)");
    }
    match args.discovery_source {
        Source::Logs => spawn_log_listener(
            args.ws.clone(),
            program,
            sig_tx,
            shutdown.clone(),
            status.clone(),
        ),
        Source::Geckoterminal => {
            tracing::info!(target: "discovery", pages = args.gecko_pages, poll_secs = args.poll_secs, "discovery source: GeckoTerminal new-pools listing (pools verified on-chain)");
            spawn_gecko_poller(
                args.gecko_url.clone(),
                args.gecko_pages,
                args.poll_secs,
                args.watch_secs * 1000,
                sig_tx,
                shutdown.clone(),
                status.clone(),
            )
        }
    }
    // Pools named by GeckoTerminal, waiting for on-chain verification
    // (bounded; a pool whose state cannot be read yet is retried a few times).
    // (pool, failed reads, earliest next read ms)
    let mut listed_pools: std::collections::VecDeque<(Pubkey, u8, u64)> = Default::default();

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
        while let Ok(ev) = sig_rx.try_recv() {
            match ev {
                DiscoveryEvent::Signature(sig) => {
                    if pending.push(sig) {
                        gap(&status, &status.signatures_evicted);
                        tracing::warn!(target: "discovery", "pending signature queue full — oldest CreatePool dropped");
                    }
                }
                DiscoveryEvent::Pool(pk) => {
                    if listed_pools.len() >= LISTED_POOLS_CAP {
                        listed_pools.pop_front();
                        gap(&status, &status.signatures_evicted);
                    }
                    listed_pools.push_back((pk, 0, 0));
                }
            }
        }
        // GeckoTerminal is only a hint: the pool account itself must be a
        // CLMM PoolState (owner + discriminator) before anything uses it.
        let mut reads = 0;
        for _ in 0..listed_pools.len() {
            if reads >= 5 {
                break;
            }
            let Some((pk, failures, next_ms)) = listed_pools.pop_front() else {
                break;
            };
            if next_ms > now_ms() {
                listed_pools.push_back((pk, failures, next_ms));
                continue;
            }
            reads += 1;
            match account_resolver::fetch_pool_state(&rpc, &pk, &program) {
                Ok(state) => {
                    let p = NewPool {
                        pool_id: pk,
                        mint_0: state.token_mint_0,
                        mint_1: state.token_mint_1,
                    };
                    match token_side(&p, &wsol) {
                        Some(token) => {
                            if watch.add(p, token, now_ms()) {
                                bump(&status.pools_watched);
                                tracing::info!(target: "discovery", pool = %p.pool_id, token = %token, watched = watch.len(), "new wSOL pool on watch list");
                            }
                        }
                        None => bump(&status.pools_not_wsol),
                    }
                }
                Err(e) if transient_pool_read_error(&e) => {
                    let failures = failures.saturating_add(1);
                    match listed_retry_delay_ms(failures) {
                        Some(delay) => {
                            tracing::warn!(target: "discovery", pool = %pk, error = %e, retry_in_secs = delay / 1000, "listed pool could not be read (RPC) — will retry");
                            listed_pools.push_back((pk, failures, now_ms() + delay));
                        }
                        None => {
                            gap(&status, &status.signatures_unresolved);
                            tracing::warn!(target: "discovery", pool = %pk, error = %e, "listed pool still unreadable after all retries — dropped");
                        }
                    }
                }
                Err(e) => {
                    // Read fine but not a CLMM PoolState: final, not a gap.
                    bump(&status.pools_rejected);
                    tracing::warn!(target: "discovery", pool = %pk, error = %e, "listed pool failed on-chain verification — rejected");
                }
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
    fn stream_down_alarm_fires_after_ten_minutes_and_repeats() {
        let m = STREAM_DOWN_ALARM_MS;
        // Never received anything since start.
        assert_eq!(silence_alarm_due(0, 1_000, 0, 1_000 + m - 1), None);
        assert_eq!(silence_alarm_due(0, 1_000, 0, 1_000 + m), Some(m));
        // Last message long ago; previous alarm too recent, then due again.
        let last = 5_000_000;
        assert_eq!(silence_alarm_due(last, 0, last + m, last + m + 1), None);
        assert_eq!(
            silence_alarm_due(last, 0, last + m, last + 2 * m),
            Some(2 * m)
        );
        // A live stream never alarms.
        assert_eq!(silence_alarm_due(last, 0, 0, last + 1_000), None);
    }

    #[test]
    fn error_chain_shows_the_cause_and_redacts_keys() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "websocket error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = Outer(std::io::Error::other(
            "HTTP error: 429 Too Many Requests for wss://x.example/?api-key=SECRET123",
        ));
        let s = error_chain(&e);
        assert!(s.starts_with("websocket error: HTTP error: 429"), "{s}");
        assert!(!s.contains("SECRET123"), "{s}");
    }

    #[test]
    fn gecko_source_is_opt_in_and_validated() {
        let d = args(&[]);
        assert_eq!(d.discovery_source, Source::Logs, "default unchanged");
        assert!(validate(&d).is_ok());
        let g = args(&["--discovery-source", "geckoterminal"]);
        assert_eq!((g.gecko_pages, g.poll_secs), (3, 60));
        assert!(
            g.gecko_url.starts_with("https://api.geckoterminal.com/")
                && g.gecko_url.contains("{page}")
        );
        assert!(validate(&g).is_ok());
        for bad in [
            &["--discovery-source", "geckoterminal", "--poll-secs", "10"][..],
            &["--discovery-source", "geckoterminal", "--gecko-pages", "0"][..],
            &["--discovery-source", "geckoterminal", "--gecko-pages", "11"][..],
            &[
                "--discovery-source",
                "geckoterminal",
                "--gecko-url",
                "http://x/?page={page}",
            ][..],
            &[
                "--discovery-source",
                "geckoterminal",
                "--gecko-url",
                "https://x/pools",
            ][..],
        ] {
            assert!(validate(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn listed_pool_reads_are_retried_spaced_then_dropped() {
        assert_eq!(listed_retry_delay_ms(0), None, "no failure yet: no delay");
        assert_eq!(listed_retry_delay_ms(1), Some(30_000));
        assert_eq!(listed_retry_delay_ms(4), Some(1_200_000));
        assert_eq!(listed_retry_delay_ms(5), None, "retries used up");
        assert_eq!(LISTED_POOL_RETRY_MS.iter().sum::<u64>(), 1_950_000);
        // RPC failures retry; a pool that was read and is not a CLMM pool is final.
        assert!(transient_pool_read_error(
            "failed to fetch pool state X: AccountNotFound: pubkey=X: HTTP status client error (429 Too Many Requests)"
        ));
        assert!(!transient_pool_read_error(
            "PoolState X is owned by Y, not the CLMM program"
        ));
    }

    #[test]
    fn gecko_gap_when_pages_do_not_reach_the_previous_poll() {
        assert!(!gecko_gap(None, Some(5)), "first poll");
        assert!(!gecko_gap(Some(100), None), "empty pages");
        assert!(
            !gecko_gap(Some(100), Some(90)),
            "pages reach back before the last poll"
        );
        assert!(!gecko_gap(Some(100), Some(100)));
        assert!(
            gecko_gap(Some(100), Some(101)),
            "every listed pool is newer: some may be missing"
        );
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
