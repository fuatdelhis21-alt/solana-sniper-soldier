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
use std::sync::Arc;

use prometheus::{
    Encoder, Gauge, Histogram, HistogramOpts, IntCounter, IntCounterVec, Opts, Registry,
    TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Shared metrics registry + counters, cheaply cloneable across tasks.
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub trades_total: IntCounter,
    pub trades_success: IntCounter,
    pub trades_failed: IntCounter,
    pub hsm_requests: IntCounter,
    pub hsm_errors: IntCounter,
    pub latency_hist: Histogram,
    pub hsm_latency_hist: Histogram,
    pub kill_switch_active: Gauge,
    pub last_trade_ts: Gauge,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

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

        registry.register(Box::new(trades_total.clone())).ok();
        registry.register(Box::new(trades_success.clone())).ok();
        registry.register(Box::new(trades_failed.clone())).ok();
        registry.register(Box::new(hsm_requests.clone())).ok();
        registry.register(Box::new(hsm_errors.clone())).ok();
        registry.register(Box::new(latency_hist.clone())).ok();
        registry.register(Box::new(hsm_latency_hist.clone())).ok();
        registry.register(Box::new(kill_switch_active.clone())).ok();
        registry.register(Box::new(last_trade_ts.clone())).ok();
        Arc::new(Self {
            registry,
            trades_total,
            trades_success,
            trades_failed,
            hsm_requests,
            hsm_errors,
            latency_hist,
            hsm_latency_hist,
            kill_switch_active,
            last_trade_ts,
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
}
