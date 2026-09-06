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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use prometheus::{
    Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounter, IntCounterVec, Opts, Registry,
    TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Readiness snapshot for the `/ready` endpoint. Distinct from process
/// liveness (`/health`): a process can be alive yet NOT trade-ready (e.g.
/// circuit breaker Open, RECONCILE_REQUIRED, EXIT_BLOCKED, HSM down).
/// `main.rs` refreshes this every loop iteration via `set_readiness`.
#[derive(Debug, Clone, Default)]
pub struct ReadinessSnapshot {
    /// Process is alive and the metrics server is serving.
    pub process_alive: bool,
    /// Risk state file parsed successfully at startup.
    pub state_verified: bool,
    /// RECONCILE_REQUIRED flag (operator must act; entries blocked).
    pub reconcile_required: bool,
    /// EXIT_BLOCKED flag (exit retry budget exhausted; position stays open).
    pub exit_blocked: bool,
    /// Circuit breaker state: 0=closed, 1=half_open, 2=open.
    pub breaker_state: u8,
    /// Kill switch active (any category).
    pub kill_switch_active: bool,
    /// Live mode is armed (--confirm-live + arm_live). False in paper/dry-run.
    pub live_armed: bool,
    /// HSM reachable + pubkey verified (live mode). Unknown in paper/dry-run.
    pub hsm_ready: Option<bool>,
    /// RPC reachable (last known). Unknown before first check.
    pub rpc_ready: Option<bool>,
    /// Market data feed available (WS or RPC poll). Unknown before first check.
    pub market_data_ready: Option<bool>,
    /// Current operating mode: "paper" | "dry_run" | "live" | "simulation".
    pub mode: String,
}

impl ReadinessSnapshot {
    /// Whether the bot is ready to run a PAPER/DRY-RUN iteration (no live
    /// arm, no HSM, no on-chain requirement).
    pub fn ready_paper_dry_run(&self) -> bool {
        self.process_alive && self.state_verified && !self.reconcile_required
    }

    /// Whether the bot is ready to run a LIVE iteration. Every fail-closed
    /// gate must be satisfied; a single false means NOT trade-ready.
    pub fn ready_live(&self) -> bool {
        self.process_alive
            && self.state_verified
            && !self.reconcile_required
            && !self.exit_blocked
            && self.breaker_state != 2 // not Open
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
    /// Latest readiness snapshot, refreshed by `main.rs` each iteration and
    /// served by the `/ready` endpoint.
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
    pub trade_attempt_total: IntCounter,
    /// Rejections, labeled by machine-readable `RejectReason::code()`
    /// (stale_price, slippage_exceeded, max_spend_exceeded,
    /// daily_trade_cap, daily_loss_limit, circuit_breaker_open,
    /// token_authority_risk, holder_concentration_exceeded,
    /// insufficient_liquidity, hsm_unavailable, ...).
    pub trade_rejected_total: IntCounterVec,
    /// Trades that were actually built, signed, and submitted successfully.
    pub trade_executed_total: IntCounter,
    /// Circuit breaker state gauge (1=open/tripped, 0=closed/normal).
    pub circuit_breaker_state: Gauge,
    /// Realized P&L for the current UTC day, in lamports (loss-only today —
    /// see final report for the known gap: no exit-execution path exists to
    /// compute realized wins).
    pub daily_realized_pnl: Gauge,
    /// Number of currently tracked open positions.
    pub open_position_count: Gauge,
    /// EXIT-MANAGEMENT observability (AŞAMA 4/5/6): exit submissions,
    /// rejections by reason, executions, and the EXIT_BLOCKED / retry state.
    pub trade_exit_attempt_total: IntCounter,
    pub trade_exit_rejected_total: IntCounterVec,
    pub trade_exit_executed_total: IntCounter,
    /// Closed positions (exits) today — never gates anything.
    pub daily_exits: Gauge,
    /// Kill switch tri-state per category: {reason="integrity"|"risk_limit"|"unknown"}
    /// is 1 when the kill switch is active with that category, else 0.
    pub kill_switch_state: GaugeVec,
    /// Circuit breaker state machine: 0=closed, 1=half_open, 2=open.
    pub breaker_state: Gauge,
    /// EXIT_BLOCKED flag (exit retry budget exhausted; position stays open).
    pub exit_blocked: Gauge,
    /// Age in seconds of the currently open position (0 when none).
    pub open_position_age_seconds: Gauge,
    /// RECONCILE_REQUIRED flag (startup orphan/mismatch; operator action).
    pub reconcile_required: Gauge,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();
        let readiness = Arc::new(Mutex::new(ReadinessSnapshot::default()));

        let trades_total = IntCounter::new("hft_trades_total", "Total trade attempts").unwrap();
        let trades_success = IntCounter::new("hft_trades_success", "Successful trades").unwrap();
        let trades_failed = IntCounter::new("hft_trades_failed", "Failed trades").unwrap();
        let hsm_requests = IntCounter::new("hft_hsm_requests", "HSM sign requests").unwrap();
        let hsm_errors = IntCounter::new("hft_hsm_errors", "HSM sign errors").unwrap();
        let latency_hist = Histogram::with_opts(HistogramOpts::new(
            "hft_trade_latency_ms",
            "Trade latency in milliseconds",
        ))
        .unwrap();
        let hsm_latency_hist = Histogram::with_opts(HistogramOpts::new(
            "hft_hsm_latency_ms",
            "HSM sign latency in milliseconds",
        ))
        .unwrap();
        let kill_switch_active =
            Gauge::new("hft_kill_switch_active", "Kill switch state (1=active)").unwrap();
        let last_trade_ts = Gauge::new("hft_last_trade_ts", "Unix ms of last trade").unwrap();

        let trade_attempt_total =
            IntCounter::new("trade_attempt_total", "Total risk-gate evaluations").unwrap();
        let trade_rejected_total = IntCounterVec::new(
            Opts::new("trade_rejected_total", "Rejected trades by reason code"),
            &["reason"],
        )
        .unwrap();
        let trade_executed_total =
            IntCounter::new("trade_executed_total", "Trades successfully submitted").unwrap();
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
        let trade_exit_attempt_total = IntCounter::new(
            "trade_exit_attempt_total",
            "Exit transaction submission attempts (bounded retry budget)",
        )
        .unwrap();
        let trade_exit_rejected_total = IntCounterVec::new(
            Opts::new(
                "trade_exit_rejected_total",
                "Rejected exits by machine-readable reason code",
            ),
            &["reason"],
        )
        .unwrap();
        let trade_exit_executed_total = IntCounter::new(
            "trade_exit_executed_total",
            "Exits successfully submitted and confirmed",
        )
        .unwrap();
        let daily_exits = Gauge::new("daily_exits", "Closed positions (exits) today").unwrap();
        let kill_switch_state = GaugeVec::new(
            Opts::new(
                "kill_switch_state",
                "Kill switch active state by category (1=active)",
            ),
            &["reason"],
        )
        .unwrap();
        let breaker_state = Gauge::new(
            "breaker_state",
            "Circuit breaker state (0=closed, 1=half_open, 2=open)",
        )
        .unwrap();
        let exit_blocked = Gauge::new(
            "exit_blocked",
            "EXIT_BLOCKED flag (exit retry budget exhausted; position stays open)",
        )
        .unwrap();
        let open_position_age_seconds = Gauge::new(
            "open_position_age_seconds",
            "Age in seconds of the currently open position (0 when none)",
        )
        .unwrap();
        let reconcile_required = Gauge::new(
            "reconcile_required",
            "RECONCILE_REQUIRED flag (startup orphan/mismatch; operator action)",
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
            .register(Box::new(trade_exit_attempt_total.clone()))
            .ok();
        registry
            .register(Box::new(trade_exit_rejected_total.clone()))
            .ok();
        registry
            .register(Box::new(trade_exit_executed_total.clone()))
            .ok();
        registry.register(Box::new(daily_exits.clone())).ok();
        registry.register(Box::new(kill_switch_state.clone())).ok();
        registry.register(Box::new(breaker_state.clone())).ok();
        registry.register(Box::new(exit_blocked.clone())).ok();
        registry
            .register(Box::new(open_position_age_seconds.clone()))
            .ok();
        registry.register(Box::new(reconcile_required.clone())).ok();
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
            trade_exit_attempt_total,
            trade_exit_rejected_total,
            trade_exit_executed_total,
            daily_exits,
            kill_switch_state,
            breaker_state,
            exit_blocked,
            open_position_age_seconds,
            reconcile_required,
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
            let _ = socket.read(&mut buf).await;
            let req = String::from_utf8_lossy(&buf);
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();

            let (status, body) = match path.as_str() {
                "/health" => ("200 OK", "ok".to_string()),
                "/ready" => {
                    let snap = metrics.readiness.lock().unwrap().clone();
                    let ready = if snap.mode == "live" {
                        snap.ready_live()
                    } else {
                        snap.ready_paper_dry_run()
                    };
                    let body = serde_json::json!({
                        "ready": ready,
                        "mode": snap.mode,
                        "process_alive": snap.process_alive,
                        "state_verified": snap.state_verified,
                        "reconcile_required": snap.reconcile_required,
                        "exit_blocked": snap.exit_blocked,
                        "breaker_state": snap.breaker_state,
                        "kill_switch_active": snap.kill_switch_active,
                        "live_armed": snap.live_armed,
                        "hsm_ready": snap.hsm_ready,
                        "rpc_ready": snap.rpc_ready,
                        "market_data_ready": snap.market_data_ready,
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

/// Convenience: record a risk-gate evaluation attempt.
pub fn record_trade_attempt(metrics: &Metrics) {
    metrics.trade_attempt_total.inc();
}

/// Convenience: record a rejection with its machine-readable reason code
/// (e.g. `RejectReason::code()` from `risk.rs`).
pub fn record_trade_rejected(metrics: &Metrics, reason_code: &str) {
    metrics
        .trade_rejected_total
        .with_label_values(&[reason_code])
        .inc();
}

/// Convenience: record a successfully submitted trade.
pub fn record_trade_executed(metrics: &Metrics) {
    metrics.trade_executed_total.inc();
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

/// Convenience: record an exit submission attempt (bounded retry budget).
pub fn record_exit_attempt(metrics: &Metrics) {
    metrics.trade_exit_attempt_total.inc();
}

/// Convenience: record an exit rejection with its machine-readable reason
/// code (e.g. `RejectReason::code()` from `risk.rs`).
pub fn record_exit_rejected(metrics: &Metrics, reason_code: &str) {
    metrics
        .trade_exit_rejected_total
        .with_label_values(&[reason_code])
        .inc();
}

/// Convenience: record a successfully confirmed exit.
pub fn record_exit_executed(metrics: &Metrics) {
    metrics.trade_exit_executed_total.inc();
}

/// Refresh the EXIT-MANAGEMENT / integrity state gauges (AŞAMA 4/5/6):
/// breaker state machine (0=closed, 1=half_open, 2=open), kill switch
/// category state, EXIT_BLOCKED, RECONCILE_REQUIRED, daily exits and the
/// open position age. `kill_switch_reason` is the active category code
/// ("integrity"/"risk_limit") or `None` when released.
pub fn set_state_gauges(
    metrics: &Metrics,
    breaker_state: u8,
    kill_switch_active: bool,
    kill_switch_reason: Option<&str>,
    exit_blocked: bool,
    reconcile_required: bool,
    daily_exits: u64,
    open_position_age_seconds: f64,
) {
    metrics.breaker_state.set(breaker_state as f64);
    // Kill-switch tri-state: clear every category, then set the active one.
    for reason in ["integrity", "risk_limit", "unknown"] {
        metrics
            .kill_switch_state
            .with_label_values(&[reason])
            .set(0.0);
    }
    if kill_switch_active {
        let reason = kill_switch_reason.unwrap_or("unknown");
        metrics
            .kill_switch_state
            .with_label_values(&[reason])
            .set(1.0);
    }
    metrics
        .exit_blocked
        .set(if exit_blocked { 1.0 } else { 0.0 });
    metrics
        .reconcile_required
        .set(if reconcile_required { 1.0 } else { 0.0 });
    metrics.daily_exits.set(daily_exits as f64);
    metrics
        .open_position_age_seconds
        .set(open_position_age_seconds);
}

/// Refresh the readiness snapshot served by `/ready`. `main.rs` calls this
/// every loop iteration with the current risk/HSM/RPC/market-data state so
/// the endpoint reflects live conditions (not a stale startup value).
/// `process_alive` is set true once the metrics server is serving.
pub fn set_readiness(metrics: &Metrics, snap: ReadinessSnapshot) {
    *metrics.readiness.lock().unwrap() = snap;
}

/// Read the current readiness snapshot (used by tests and the `/ready`
/// handler).
pub fn readiness_snapshot(metrics: &Metrics) -> ReadinessSnapshot {
    metrics.readiness.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        record_trade_attempt(&m);
        record_trade_attempt(&m);
        record_trade_rejected(&m, "stale_price");
        record_trade_rejected(&m, "stale_price");
        record_trade_rejected(&m, "daily_loss_limit");
        record_trade_executed(&m);
        set_risk_gauges(&m, true, -150_000_000, 1);
        let out = m.render();
        assert!(out.contains("trade_attempt_total 2"));
        assert!(out.contains("trade_rejected_total{reason=\"stale_price\"} 2"));
        assert!(out.contains("trade_rejected_total{reason=\"daily_loss_limit\"} 1"));
        assert!(out.contains("trade_executed_total 1"));
        assert!(out.contains("circuit_breaker_state 1"));
        assert!(out.contains("daily_realized_pnl -150000000"));
        assert!(out.contains("open_position_count 1"));
    }

    #[test]
    fn exit_management_metrics_render() {
        let m = Metrics::new();
        record_exit_attempt(&m);
        record_exit_rejected(&m, "exit_retry_backoff");
        record_exit_rejected(&m, "exit_blocked_integrity");
        record_exit_executed(&m);
        set_state_gauges(&m, 2, true, Some("integrity"), true, false, 3, 42.5);
        let out = m.render();
        assert!(out.contains("trade_exit_attempt_total 1"));
        assert!(out.contains("trade_exit_rejected_total{reason=\"exit_retry_backoff\"} 1"));
        assert!(out.contains("trade_exit_rejected_total{reason=\"exit_blocked_integrity\"} 1"));
        assert!(out.contains("trade_exit_executed_total 1"));
        assert!(out.contains("breaker_state 2"));
        assert!(out.contains("kill_switch_state{reason=\"integrity\"} 1"));
        assert!(out.contains("kill_switch_state{reason=\"risk_limit\"} 0"));
        assert!(out.contains("exit_blocked 1"));
        assert!(out.contains("reconcile_required 0"));
        assert!(out.contains("daily_exits 3"));
        assert!(out.contains("open_position_age_seconds 42.5"));
    }

    #[test]
    fn state_gauges_reflect_half_open_and_risk_limit() {
        let m = Metrics::new();
        set_state_gauges(&m, 1, true, Some("risk_limit"), false, true, 1, 0.0);
        let out = m.render();
        assert!(out.contains("breaker_state 1"));
        assert!(out.contains("kill_switch_state{reason=\"integrity\"} 0"));
        assert!(out.contains("kill_switch_state{reason=\"risk_limit\"} 1"));
        assert!(out.contains("exit_blocked 0"));
        assert!(out.contains("reconcile_required 1"));
        // Releasing the kill switch clears the active category.
        set_state_gauges(&m, 0, false, None, false, false, 0, 0.0);
        let out = m.render();
        assert!(out.contains("breaker_state 0"));
        assert!(out.contains("kill_switch_state{reason=\"risk_limit\"} 0"));
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

        handle.abort();
    }

    #[test]
    fn readiness_paper_dry_run_requires_state_and_no_reconcile() {
        // Default snapshot: process alive, nothing else set.
        let snap = ReadinessSnapshot {
            process_alive: true,
            mode: "paper".to_string(),
            ..Default::default()
        };
        // state_verified defaults false → not ready.
        assert!(!snap.ready_paper_dry_run());
        let mut snap = snap;
        snap.state_verified = true;
        assert!(snap.ready_paper_dry_run());
        // RECONCILE_REQUIRED blocks paper/dry-run readiness.
        snap.reconcile_required = true;
        assert!(!snap.ready_paper_dry_run());
    }

    #[test]
    fn readiness_live_requires_all_fail_closed_gates() {
        let base = ReadinessSnapshot {
            process_alive: true,
            state_verified: true,
            reconcile_required: false,
            exit_blocked: false,
            breaker_state: 0,
            kill_switch_active: false,
            live_armed: true,
            hsm_ready: Some(true),
            rpc_ready: Some(true),
            market_data_ready: Some(true),
            mode: "live".to_string(),
        };
        assert!(base.ready_live());

        // Each single gate failure must make live NOT ready.
        let mut s = base.clone();
        s.state_verified = false;
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.reconcile_required = true;
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.exit_blocked = true;
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.breaker_state = 2; // Open
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.kill_switch_active = true;
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.live_armed = false;
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.hsm_ready = Some(false);
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.rpc_ready = Some(false);
        assert!(!s.ready_live());
        let mut s = base.clone();
        s.market_data_ready = Some(false);
        assert!(!s.ready_live());
        // HalfOpen (1) is live-ready (probe succeeded; trial trades allowed).
        let mut s = base.clone();
        s.breaker_state = 1;
        assert!(s.ready_live());
    }

    #[test]
    fn readiness_snapshot_set_and_read_roundtrip() {
        let m = Metrics::new();
        let snap = ReadinessSnapshot {
            process_alive: true,
            state_verified: true,
            mode: "dry_run".to_string(),
            ..Default::default()
        };
        set_readiness(&m, snap.clone());
        let got = readiness_snapshot(&m);
        assert_eq!(got.mode, "dry_run");
        assert!(got.state_verified);
        assert!(got.ready_paper_dry_run());
    }

    #[tokio::test]
    async fn ready_endpoint_reflects_snapshot() {
        let m = Metrics::new();
        // Not ready by default (state not verified).
        let addr = "127.0.0.1:0";
        let listener = TcpListener::bind(addr).await.unwrap();
        let bound = listener.local_addr().unwrap();
        drop(listener);
        let server_addr = format!("127.0.0.1:{}", bound.port());
        let m2 = m.clone();
        let sa = server_addr.clone();
        let handle = tokio::spawn(async move {
            let _ = spawn_metrics_server(&sa, m2).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Default snapshot → not ready → 503.
        let resp = reqwest::get(&format!("http://{server_addr}/ready"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);

        // Mark ready (paper mode) → 200.
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
        let body = resp.text().await.unwrap();
        assert!(body.contains("\"ready\":true"));
        assert!(body.contains("\"mode\":\"paper\""));

        handle.abort();
    }
}
