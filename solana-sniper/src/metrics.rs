//! # Observability — Prometheus metrics + health-check endpoint
//!
//! Exposes a minimal HTTP server that serves:
//! - `GET /metrics` — Prometheus text exposition of live counters/gauges.
//! - `GET /health` — liveness/readiness probe (200 OK when the bot is up).
//!
//! The server is intentionally dependency-light: it uses `tokio::net::TcpListener`
//! and `std::io` to avoid pulling in a full web framework. It is fail-closed:
//! if the metrics registry is unavailable, `/health` still reports the process
//! liveness but `/metrics` returns 503.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use prometheus::{
    Encoder, Gauge, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts,
    Registry, TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone, Default)]
pub struct ReadinessSnapshot {
    pub process_alive: bool,
    pub state_verified: bool,
    pub reconcile_required: bool,
    pub exit_blocked: bool,
    pub breaker_state: u8,
    pub kill_switch_active: bool,
    pub live_armed: bool,
    pub hsm_ready: Option<bool>,
    pub rpc_ready: Option<bool>,
    pub market_data_ready: Option<bool>,
    pub mode: String,
}

impl ReadinessSnapshot {
    pub fn ready_paper_dry_run(&self) -> bool {
        self.process_alive && self.state_verified && !self.reconcile_required
    }

    pub fn ready_live(&self) -> bool {
        self.process_alive
            && self.state_verified
            && !self.reconcile_required
            && !self.exit_blocked
            && self.breaker_state != 2
            && !self.kill_switch_active
            && self.live_armed
            && self.hsm_ready == Some(true)
            && self.rpc_ready == Some(true)
            && self.market_data_ready == Some(true)
    }
}

/// Shared metrics registry + counters, cheaply cloneable across tasks.
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    readiness: Arc<Mutex<ReadinessSnapshot>>,
    pub trades_total: IntCounter,
    pub trades_success: IntCounter,
    pub trades_failed: IntCounter,
    pub hsm_requests: IntCounter,
    pub hsm_errors: IntCounter,
    pub latency_hist: Histogram,
    pub hsm_latency_hist: Histogram,
    pub kill_switch_active: Gauge,
    pub last_trade_ts: Gauge,
    /// Every time the risk gate is evaluated for a candidate trade,
    /// regardless of outcome.
    /// Risk-gate evaluations per operating mode
    /// (`mode`: "paper" | "dry_run" | "live" | "simulation").
    pub trade_attempt_total: IntCounterVec,
    /// Rejections, labeled by operating mode and machine-readable
    /// `RejectReason::code()` (stale_price, slippage_exceeded,
    /// max_spend_exceeded, daily_trade_cap, daily_loss_limit,
    /// circuit_breaker_open, token_authority_risk,
    /// holder_concentration_exceeded, insufficient_liquidity,
    /// hsm_unavailable, ...).
    pub trade_rejected_total: IntCounterVec,
    /// Trades actually executed per mode (live: submitted on-chain; paper:
    /// simulated "would-execute" entries that passed every real gate).
    pub trade_executed_total: IntCounterVec,
    /// Circuit breaker state gauge (1=open/tripped, 0=closed/normal).
    pub circuit_breaker_state: Gauge,
    /// Realized loss for the current day, in lamports (published as a
    /// negative number). Fed by the live exit path's real balance deltas.
    pub daily_realized_pnl: Gauge,
    /// Number of currently tracked open positions.
    pub open_position_count: Gauge,
    /// Cumulative simulated P&L of closed paper positions, in lamports
    /// (negative = simulated loss). NEVER mixed with `daily_realized_pnl`
    /// which tracks real on-chain P&L only.
    pub paper_simulated_pnl: Gauge,
    /// Per-stage latency (ms); the `stage` label only takes [`Stage`]
    /// values, so its cardinality is fixed.
    pub stage_latency_ms: HistogramVec,
    /// Unix ms of the last main-loop heartbeat (see [`Metrics::heartbeat`]);
    /// shared by clones like the prometheus handles.
    heartbeat_ms: Arc<AtomicU64>,
    /// /health fails when the heartbeat is older than this; 0 = not enforced.
    health_max_age_ms: Arc<AtomicU64>,
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Health verdict from the last heartbeat: a stuck or dead trading loop
/// must not look healthy just because the HTTP server still answers.
pub fn health_status(heartbeat_ms: u64, max_age_ms: u64, now_ms: u64) -> Result<(), String> {
    if max_age_ms == 0 {
        return Ok(());
    }
    let age = now_ms.saturating_sub(heartbeat_ms);
    if age > max_age_ms {
        return Err(format!(
            "trading loop stalled: last heartbeat {age} ms ago (limit {max_age_ms} ms)"
        ));
    }
    Ok(())
}

impl Metrics {
    /// Enable the heartbeat check of /health (counts from now).
    pub fn set_health_max_age(&self, max_age_ms: u64) {
        self.heartbeat();
        self.health_max_age_ms.store(max_age_ms, Ordering::SeqCst);
    }

    /// Record that the main loop is alive (call once per iteration).
    pub fn heartbeat(&self) {
        self.heartbeat_ms.store(unix_ms(), Ordering::SeqCst);
    }

    pub fn health(&self) -> Result<(), String> {
        health_status(
            self.heartbeat_ms.load(Ordering::SeqCst),
            self.health_max_age_ms.load(Ordering::SeqCst),
            unix_ms(),
        )
    }

    pub fn new() -> Arc<Self> {
        let registry = Registry::new();
        let readiness = Arc::new(Mutex::new(ReadinessSnapshot::default()));

        let trades_total = IntCounter::new("hft_trades_total", "Total trade attempts").unwrap();
        let trades_success = IntCounter::new("hft_trades_success", "Successful trades").unwrap();
        let trades_failed = IntCounter::new("hft_trades_failed", "Failed trades").unwrap();
        let hsm_requests = IntCounter::new("hft_hsm_requests", "HSM sign requests").unwrap();
        let hsm_errors = IntCounter::new("hft_hsm_errors", "HSM sign errors").unwrap();
        // Millisecond buckets (the prometheus defaults are in seconds).
        let latency_hist = Histogram::with_opts(
            HistogramOpts::new("hft_trade_latency_ms", "Trade latency in milliseconds")
                .buckets(MS_BUCKETS.to_vec()),
        )
        .unwrap();
        let hsm_latency_hist = Histogram::with_opts(
            HistogramOpts::new("hft_hsm_latency_ms", "HSM sign latency in milliseconds")
                .buckets(MS_BUCKETS.to_vec()),
        )
        .unwrap();
        let stage_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "sniper_stage_latency_ms",
                "Per-stage latency in milliseconds",
            )
            .buckets(MS_BUCKETS.to_vec()),
            &["stage"],
        )
        .unwrap();
        let kill_switch_active =
            Gauge::new("hft_kill_switch_active", "Kill switch state (1=active)").unwrap();
        let last_trade_ts = Gauge::new("hft_last_trade_ts", "Unix ms of last trade").unwrap();

        let trade_attempt_total = IntCounterVec::new(
            Opts::new("trade_attempt_total", "Total risk-gate evaluations"),
            &["mode"],
        )
        .unwrap();
        let trade_rejected_total = IntCounterVec::new(
            Opts::new("trade_rejected_total", "Rejected trades by reason code"),
            &["mode", "reason"],
        )
        .unwrap();
        let trade_executed_total = IntCounterVec::new(
            Opts::new(
                "trade_executed_total",
                "Trades successfully submitted (live) or simulated as executed (paper)",
            ),
            &["mode"],
        )
        .unwrap();
        let circuit_breaker_state = Gauge::new(
            "circuit_breaker_state",
            "Circuit breaker state (1=open/tripped, 0=closed)",
        )
        .unwrap();
        let daily_realized_pnl = Gauge::new(
            "daily_realized_pnl",
            "Realized P&L for the current day, in lamports",
        )
        .unwrap();
        let open_position_count =
            Gauge::new("open_position_count", "Currently tracked open positions").unwrap();
        let paper_simulated_pnl = Gauge::new(
            "paper_simulated_pnl",
            "Cumulative simulated P&L of closed paper positions, in lamports",
        )
        .unwrap();

        registry.register(Box::new(trades_total.clone())).ok();
        registry.register(Box::new(trades_success.clone())).ok();
        registry.register(Box::new(trades_failed.clone())).ok();
        registry.register(Box::new(hsm_requests.clone())).ok();
        registry.register(Box::new(hsm_errors.clone())).ok();
        registry.register(Box::new(latency_hist.clone())).ok();
        registry.register(Box::new(hsm_latency_hist.clone())).ok();
        registry.register(Box::new(kill_switch_active.clone())).ok();
        registry.register(Box::new(last_trade_ts.clone())).ok();
        registry
            .register(Box::new(trade_attempt_total.clone()))
            .ok();
        registry
            .register(Box::new(trade_rejected_total.clone()))
            .ok();
        registry
            .register(Box::new(trade_executed_total.clone()))
            .ok();
        registry
            .register(Box::new(circuit_breaker_state.clone()))
            .ok();
        registry.register(Box::new(daily_realized_pnl.clone())).ok();
        registry
            .register(Box::new(open_position_count.clone()))
            .ok();
        registry
            .register(Box::new(paper_simulated_pnl.clone()))
            .ok();
        registry.register(Box::new(stage_latency_ms.clone())).ok();
        Arc::new(Self {
            registry,
            readiness,
            trades_total,
            trades_success,
            trades_failed,
            hsm_requests,
            hsm_errors,
            latency_hist,
            hsm_latency_hist,
            kill_switch_active,
            last_trade_ts,
            trade_attempt_total,
            trade_rejected_total,
            trade_executed_total,
            circuit_breaker_state,
            daily_realized_pnl,
            open_position_count,
            paper_simulated_pnl,
            stage_latency_ms,
            heartbeat_ms: Arc::new(AtomicU64::new(0)),
            health_max_age_ms: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Render the Prometheus text exposition.
    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        if encoder.encode(&self.registry.gather(), &mut buf).is_ok() {
            String::from_utf8_lossy(&buf).into_owned()
        } else {
            String::new()
        }
    }
}

/// Spawn the metrics/health HTTP server on `addr`. Returns the listener task.
pub async fn spawn_metrics_server(addr: &str, metrics: Arc<Metrics>) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("metrics server bind failed on {addr}: {e}"))?;
    tracing::info!(target: "metrics", addr = %addr, "metrics/health server listening");

    loop {
        let (mut socket, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "metrics", err = %e, "accept failed");
                continue;
            }
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            // A client that never sends its request must not hold the task.
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut buf))
                    .await;
            let n = match read {
                Ok(Ok(n)) => n,
                _ => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();

            let (status, body) = match path.as_str() {
                "/health" => match metrics.health() {
                    Ok(()) => ("200 OK", "ok".to_string()),
                    Err(e) => ("503 Service Unavailable", e),
                },
                "/ready" => {
                    let snapshot = metrics.readiness.lock().unwrap().clone();
                    let ready = if snapshot.mode == "live" {
                        snapshot.ready_live()
                    } else {
                        snapshot.ready_paper_dry_run()
                    };
                    let body = serde_json::json!({
                        "ready": ready,
                        "mode": snapshot.mode,
                        "process_alive": snapshot.process_alive,
                        "state_verified": snapshot.state_verified,
                        "reconcile_required": snapshot.reconcile_required,
                        "exit_blocked": snapshot.exit_blocked,
                        "breaker_state": snapshot.breaker_state,
                        "kill_switch_active": snapshot.kill_switch_active,
                        "live_armed": snapshot.live_armed,
                        "hsm_ready": snapshot.hsm_ready,
                        "rpc_ready": snapshot.rpc_ready,
                        "market_data_ready": snapshot.market_data_ready,
                    })
                    .to_string();
                    if ready {
                        ("200 OK", body)
                    } else {
                        ("503 Service Unavailable", body)
                    }
                }
                "/metrics" => ("200 OK", metrics.render()),
                _ => ("404 Not Found", "not found".to_string()),
            };

            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

/// Histogram buckets for millisecond latencies (1 ms .. 3 min).
const MS_BUCKETS: [f64; 16] = [
    1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0,
    30_000.0, 60_000.0, 180_000.0,
];

/// Pipeline stages with a latency histogram. A closed set: metric label
/// cardinality cannot grow with pools, reasons or errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Market data fetch (pool state / vaults / risk data).
    MarketData,
    /// Strategy + signal + risk gates.
    Decision,
    /// Exact quote by on-chain simulation.
    QuoteSim,
    /// HSM signing round trip.
    Sign,
    /// Send + confirmation (until landed or given up).
    SubmitConfirm,
    /// Whole exit attempt (quote, sign, submit, confirm).
    Exit,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::MarketData => "market_data",
            Stage::Decision => "decision",
            Stage::QuoteSim => "quote_sim",
            Stage::Sign => "sign",
            Stage::SubmitConfirm => "submit_confirm",
            Stage::Exit => "exit",
        }
    }
}

/// Record one stage duration.
pub fn observe_stage(metrics: &Metrics, stage: Stage, ms: f64) {
    metrics
        .stage_latency_ms
        .with_label_values(&[stage.label()])
        .observe(ms);
}

/// Convenience: record a trade outcome and latency.
pub fn record_trade(metrics: &Metrics, success: bool, latency_ms: f64) {
    metrics.trades_total.inc();
    metrics.latency_hist.observe(latency_ms);
    if success {
        metrics.trades_success.inc();
    } else {
        metrics.trades_failed.inc();
    }
    metrics.last_trade_ts.set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64,
    );
}

/// Convenience: record an HSM sign request and its latency.
pub fn record_hsm(metrics: &Metrics, ok: bool, latency_ms: f64) {
    metrics.hsm_requests.inc();
    metrics.hsm_latency_hist.observe(latency_ms);
    if !ok {
        metrics.hsm_errors.inc();
    }
}

/// Convenience: set the kill-switch gauge from the risk manager state.
pub fn set_kill_switch(metrics: &Metrics, active: bool) {
    metrics
        .kill_switch_active
        .set(if active { 1.0 } else { 0.0 });
}

/// Convenience: record a risk-gate evaluation attempt for an operating
/// mode (`mode`: "paper" | "dry_run" | "live" | "simulation").
pub fn record_trade_attempt(metrics: &Metrics, mode: &str) {
    metrics.trade_attempt_total.with_label_values(&[mode]).inc();
}

/// Convenience: record a rejection with its operating mode and
/// machine-readable reason code (e.g. `RejectReason::code()` from `risk.rs`).
pub fn record_trade_rejected(metrics: &Metrics, mode: &str, reason_code: &str) {
    metrics
        .trade_rejected_total
        .with_label_values(&[mode, reason_code])
        .inc();
}

/// Convenience: record an executed trade for an operating mode. In live mode
/// this means submitted+confirmed on-chain; in paper mode it means a
/// simulated "would-execute" entry that passed every real gate.
pub fn record_trade_executed(metrics: &Metrics, mode: &str) {
    metrics
        .trade_executed_total
        .with_label_values(&[mode])
        .inc();
}

/// Convenience: refresh the circuit-breaker/open-position/PNL gauges from
/// the current risk manager state.
pub fn set_risk_gauges(
    metrics: &Metrics,
    circuit_breaker_active: bool,
    daily_realized_pnl_lamports: i64,
    open_position_count: u64,
) {
    metrics
        .circuit_breaker_state
        .set(if circuit_breaker_active { 1.0 } else { 0.0 });
    metrics
        .daily_realized_pnl
        .set(daily_realized_pnl_lamports as f64);
    metrics.open_position_count.set(open_position_count as f64);
}

/// Convenience: publish the cumulative simulated P&L of closed paper
/// positions (AŞAMA 4). Always labeled simulated — never mixed with real
/// on-chain realized P&L.
pub fn set_paper_simulated_pnl(metrics: &Metrics, pnl_lamports: i64) {
    metrics.paper_simulated_pnl.set(pnl_lamports as f64);
}

pub fn set_readiness(metrics: &Metrics, snapshot: ReadinessSnapshot) {
    *metrics.readiness.lock().unwrap() = snapshot;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_latency_is_exported_with_bounded_labels() {
        let m = Metrics::new();
        for st in [
            Stage::MarketData,
            Stage::Decision,
            Stage::QuoteSim,
            Stage::Sign,
            Stage::SubmitConfirm,
            Stage::Exit,
        ] {
            observe_stage(&m, st, 12.0);
        }
        let out = m.render();
        assert!(out.contains("sniper_stage_latency_ms_bucket{stage=\"quote_sim\",le=\"25\"} 1"));
        assert_eq!(out.matches("sniper_stage_latency_ms_count{").count(), 6);
        record_hsm(&m, true, 40.0);
        assert!(m
            .render()
            .contains("hft_hsm_latency_ms_bucket{le=\"50\"} 1"));
    }

    #[test]
    fn metrics_render_contains_counters() {
        let m = Metrics::new();
        record_trade(&m, true, 12.5);
        record_trade(&m, false, 30.0);
        record_hsm(&m, true, 5.0);
        let out = m.render();
        assert!(out.contains("hft_trades_total 2"));
        assert!(out.contains("hft_trades_success 1"));
        assert!(out.contains("hft_trades_failed 1"));
        assert!(out.contains("hft_hsm_requests 1"));
    }

    #[test]
    fn kill_switch_gauge_reflects_state() {
        let m = Metrics::new();
        set_kill_switch(&m, true);
        assert!(m.render().contains("hft_kill_switch_active 1"));
        set_kill_switch(&m, false);
        assert!(m.render().contains("hft_kill_switch_active 0"));
    }

    #[test]
    fn risk_metrics_render_with_reason_label() {
        let m = Metrics::new();
        record_trade_attempt(&m, "paper");
        record_trade_attempt(&m, "live");
        record_trade_rejected(&m, "paper", "stale_price");
        record_trade_rejected(&m, "paper", "stale_price");
        record_trade_rejected(&m, "live", "daily_loss_limit");
        record_trade_executed(&m, "paper");
        set_risk_gauges(&m, true, -150_000_000, 1);
        let out = m.render();
        // Mode-labeled counters: paper and live attempts are separated.
        assert!(out.contains("trade_attempt_total{mode=\"paper\"} 1"));
        assert!(out.contains("trade_attempt_total{mode=\"live\"} 1"));
        assert!(out.contains("trade_rejected_total{mode=\"paper\",reason=\"stale_price\"} 2"));
        assert!(out.contains("trade_rejected_total{mode=\"live\",reason=\"daily_loss_limit\"} 1"));
        assert!(out.contains("trade_executed_total{mode=\"paper\"} 1"));
        assert!(out.contains("circuit_breaker_state 1"));
        assert!(out.contains("daily_realized_pnl -150000000"));
        assert!(out.contains("open_position_count 1"));
    }

    #[test]
    fn health_follows_the_loop_heartbeat() {
        assert!(health_status(0, 0, 10_000_000).is_ok(), "not enforced");
        assert!(health_status(1_000, 5_000, 6_000).is_ok());
        assert!(health_status(1_000, 5_000, 6_001).is_err());
        let m = Metrics::new();
        m.set_health_max_age(60_000);
        assert!(m.health().is_ok());
        m.heartbeat_ms.store(1, Ordering::SeqCst);
        assert!(m.health().unwrap_err().contains("stalled"));
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let m = Metrics::new();
        let addr = "127.0.0.1:0";
        // Bind to an ephemeral port to avoid conflicts.
        let listener = TcpListener::bind(addr).await.unwrap();
        let bound = listener.local_addr().unwrap();
        drop(listener);

        let server_addr = format!("127.0.0.1:{}", bound.port());
        let m2 = m.clone();
        let server_addr_for_task = server_addr.clone();
        let handle = tokio::spawn(async move {
            let _ = spawn_metrics_server(&server_addr_for_task, m2).await;
        });

        // Give the server a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let resp = reqwest::get(&format!("http://{server_addr}/health"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "ok");

        // A stalled loop turns /health into a 503.
        m.set_health_max_age(1);
        m.heartbeat_ms.store(1, Ordering::SeqCst);
        let resp = reqwest::get(&format!("http://{server_addr}/health"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);

        // An idle connection is dropped instead of blocking the server.
        let _idle = tokio::net::TcpStream::connect(&server_addr).await.unwrap();
        let resp = reqwest::get(&format!("http://{server_addr}/metrics"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let resp = reqwest::get(&format!("http://{server_addr}/ready"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
        set_readiness(
            &m,
            ReadinessSnapshot {
                process_alive: true,
                state_verified: true,
                mode: "paper".to_string(),
                ..Default::default()
            },
        );
        let resp = reqwest::get(&format!("http://{server_addr}/ready"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }

    #[test]
    fn live_readiness_requires_every_gate() {
        let snapshot = ReadinessSnapshot {
            process_alive: true,
            state_verified: true,
            live_armed: true,
            hsm_ready: Some(true),
            rpc_ready: Some(true),
            market_data_ready: Some(true),
            mode: "live".to_string(),
            ..Default::default()
        };
        assert!(snapshot.ready_live());
        let mut blocked = snapshot;
        blocked.breaker_state = 2;
        assert!(!blocked.ready_live());
    }
}
