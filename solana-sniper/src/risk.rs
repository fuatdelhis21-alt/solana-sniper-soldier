//! Risk management: circuit breaker, max trade size, slippage guard, daily loss
//! limit, daily trade cap, and a manual/automatic kill switch.
//!
//! ## Kill switch
//! The kill switch is a fail-closed mechanism: once triggered, *every* new
//! trade is rejected until it is explicitly released. It can be triggered
//! manually (`trigger_kill_switch`) or automatically when a configured limit
//! (daily loss, daily trade cap) is exceeded. Every trigger is written to the
//! risk audit log.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

// ============================================================================
// RiskFirewall — lightweight pre-trade guard for emergency stop & circuit breaker
// ============================================================================

/// Risk firewall with circuit breaker. Complements `RiskManager` with a
/// simpler f64-based interface for quick checks.
pub struct RiskFirewall {
    pub max_trade_sol: f64,
    pub daily_loss_limit_sol: f64,
    pub current_loss_sol: f64,
}

impl RiskFirewall {
    pub fn new(max_trade_sol: f64, daily_loss_limit_sol: f64) -> Self {
        Self {
            max_trade_sol,
            daily_loss_limit_sol,
            current_loss_sol: 0.0,
        }
    }

    pub fn pre_trade_check(&self, amount_sol: f64) -> Result<(), String> {
        if self.current_loss_sol >= self.daily_loss_limit_sol {
            return Err(
                "CIRCUIT BREAKER: Günlük zarar limitine ulaşıldı! Bot durduruldu.".to_string(),
            );
        }
        if amount_sol > self.max_trade_sol {
            return Err("RISK: Max işlem limitinin üzerinde!".to_string());
        }
        Ok(())
    }
}

/// Risk configuration.
pub struct RiskConfig {
    pub max_trade_size_lamports: u64,
    pub max_slippage_bps: u64,
    pub daily_loss_limit_lamports: u64,
    pub max_daily_trades: u64,
    pub circuit_breaker_duration: Duration,
    data_dir: PathBuf,
}

impl RiskConfig {
    pub fn devnet_defaults(data_dir: PathBuf) -> Self {
        Self {
            max_trade_size_lamports: 10_000_000_000,   // 10 SOL
            max_slippage_bps: 100,                     // 1%
            daily_loss_limit_lamports: 50_000_000_000, // 50 SOL
            max_daily_trades: 20,
            circuit_breaker_duration: Duration::from_secs(300),
            data_dir,
        }
    }
}

/// Per-day loss tracker.
#[derive(Default)]
struct DailyLoss {
    date: String,
    loss_lamports: u64,
}

/// Per-day trade counter.
#[derive(Default)]
struct DailyTrades {
    date: String,
    count: u64,
}

/// Audit log entry for a kill-switch event.
#[derive(Serialize)]
struct KillSwitchAudit {
    ts_ms: u64,
    event: &'static str,
    reason: String,
}

/// Risk manager with circuit breaker, daily trade cap, and kill switch.
pub struct RiskManager {
    config: RiskConfig,
    daily_loss: Mutex<DailyLoss>,
    daily_trades: Mutex<DailyTrades>,
    circuit_breaker_active: Mutex<bool>,
    last_breaker_reset: Mutex<Instant>,
    kill_switch_active: Mutex<bool>,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            daily_loss: Mutex::new(DailyLoss::default()),
            daily_trades: Mutex::new(DailyTrades::default()),
            circuit_breaker_active: Mutex::new(false),
            last_breaker_reset: Mutex::new(Instant::now()),
            kill_switch_active: Mutex::new(false),
        }
    }

    fn today() -> String {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    }

    fn now_ms() -> u64 {
        chrono::Local::now().timestamp_millis() as u64
    }

    /// Append a kill-switch event to the risk audit log (JSONL).
    fn write_audit(&self, event: &'static str, reason: &str) {
        let entry = KillSwitchAudit {
            ts_ms: Self::now_ms(),
            event,
            reason: reason.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            let dir = self.config.data_dir.join("audit");
            if std::fs::create_dir_all(&dir).is_ok() {
                let path = dir.join("risk_audit.jsonl");
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    /// Manually trigger the kill switch. All new trades are rejected until
    /// `release_kill_switch` is called. The event is written to the audit log.
    pub fn trigger_kill_switch(&self, reason: &str) {
        *self.kill_switch_active.lock().unwrap() = true;
        self.write_audit("kill_switch_triggered", reason);
    }

    /// Manually release the kill switch.
    pub fn release_kill_switch(&self) {
        *self.kill_switch_active.lock().unwrap() = false;
        self.write_audit("kill_switch_released", "manual release");
    }

    pub fn is_kill_switch_active(&self) -> bool {
        *self.kill_switch_active.lock().unwrap()
    }

    /// Pre-trade check: kill switch, circuit breaker, size, slippage, daily
    /// loss, and daily trade cap.
    pub fn pre_trade_check(
        &self,
        trade_size_lamports: u64,
        slippage_bps: u64,
    ) -> Result<(), String> {
        if *self.kill_switch_active.lock().unwrap() {
            return Err("KILL SWITCH ACTIVE: tüm yeni işlemler reddedildi".into());
        }
        if *self.circuit_breaker_active.lock().unwrap() {
            return Err("circuit breaker active".into());
        }
        if trade_size_lamports > self.config.max_trade_size_lamports {
            return Err(format!(
                "trade size {} exceeds max {}",
                trade_size_lamports, self.config.max_trade_size_lamports
            ));
        }
        if slippage_bps > self.config.max_slippage_bps {
            return Err(format!(
                "slippage {} bps exceeds max {} bps",
                slippage_bps, self.config.max_slippage_bps
            ));
        }
        let daily = self.daily_loss.lock().unwrap();
        if daily.loss_lamports > self.config.daily_loss_limit_lamports {
            return Err("daily loss limit exceeded".into());
        }
        let trades = self.daily_trades.lock().unwrap();
        if trades.count >= self.config.max_daily_trades {
            return Err(format!(
                "daily trade cap reached ({} >= {})",
                trades.count, self.config.max_daily_trades
            ));
        }
        Ok(())
    }

    /// Record a completed trade. If the daily trade cap is exceeded, the kill
    /// switch is triggered automatically and the event is audited.
    pub fn record_trade(&self) {
        let mut trades = self.daily_trades.lock().unwrap();
        let today = Self::today();
        if trades.date != today {
            trades.date = today;
            trades.count = 0;
        }
        trades.count += 1;
        if trades.count > self.config.max_daily_trades {
            drop(trades);
            self.trigger_kill_switch(&format!(
                "daily trade cap exceeded ({} > {})",
                self.config.max_daily_trades, self.config.max_daily_trades
            ));
        }
    }

    pub fn current_daily_trades(&self) -> u64 {
        let trades = self.daily_trades.lock().unwrap();
        if trades.date != Self::today() {
            return 0;
        }
        trades.count
    }

    /// Record a loss and potentially trigger circuit breaker.
    pub fn record_loss(&self, lamports: u64) -> Result<(), String> {
        let mut daily = self.daily_loss.lock().unwrap();
        let today = Self::today();
        if daily.date != today {
            daily.date = today;
            daily.loss_lamports = 0;
        }
        daily.loss_lamports = daily.loss_lamports.saturating_add(lamports);
        if daily.loss_lamports > self.config.daily_loss_limit_lamports {
            drop(daily);
            self.trigger_kill_switch(&format!(
                "daily loss limit exceeded ({} > {})",
                self.config.daily_loss_limit_lamports, self.config.daily_loss_limit_lamports
            ));
            return Err("daily loss limit exceeded — kill switch triggered".into());
        }
        Ok(())
    }

    pub fn current_daily_loss(&self) -> u64 {
        let daily = self.daily_loss.lock().unwrap();
        if daily.date != Self::today() {
            return 0;
        }
        daily.loss_lamports
    }

    pub fn is_circuit_breaker_active(&self) -> bool {
        let active = *self.circuit_breaker_active.lock().unwrap();
        if active {
            let elapsed = self.last_breaker_reset.lock().unwrap().elapsed();
            if elapsed > self.config.circuit_breaker_duration {
                *self.circuit_breaker_active.lock().unwrap() = false;
                return false;
            }
        }
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RiskConfig {
        RiskConfig {
            max_trade_size_lamports: 1_000_000_000,
            max_slippage_bps: 100,
            daily_loss_limit_lamports: 5_000_000_000,
            max_daily_trades: 3,
            circuit_breaker_duration: Duration::from_secs(300),
            data_dir: PathBuf::from(std::env::temp_dir()),
        }
    }

    #[test]
    fn pre_trade_check_ok_within_limits() {
        let rm = RiskManager::new(test_config());
        assert!(rm.pre_trade_check(500_000_000, 50).is_ok());
    }

    #[test]
    fn pre_trade_check_rejects_oversize() {
        let rm = RiskManager::new(test_config());
        assert!(rm.pre_trade_check(2_000_000_000, 50).is_err());
    }

    #[test]
    fn pre_trade_check_rejects_high_slippage() {
        let rm = RiskManager::new(test_config());
        assert!(rm.pre_trade_check(500_000_000, 200).is_err());
    }

    #[test]
    fn kill_switch_blocks_trades() {
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("test");
        assert!(rm.is_kill_switch_active());
        assert!(rm.pre_trade_check(500_000_000, 50).is_err());
    }

    #[test]
    fn kill_switch_release_allows_trades() {
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("test");
        rm.release_kill_switch();
        assert!(!rm.is_kill_switch_active());
        assert!(rm.pre_trade_check(500_000_000, 50).is_ok());
    }

    #[test]
    fn daily_trade_cap_triggers_kill_switch() {
        let rm = RiskManager::new(test_config());
        // max_daily_trades = 3; record 4 trades -> cap exceeded -> kill switch.
        rm.record_trade();
        rm.record_trade();
        rm.record_trade();
        assert!(!rm.is_kill_switch_active());
        rm.record_trade();
        assert!(rm.is_kill_switch_active());
        assert!(rm.pre_trade_check(500_000_000, 50).is_err());
    }

    #[test]
    fn daily_loss_triggers_kill_switch() {
        let rm = RiskManager::new(test_config());
        // daily_loss_limit = 5 SOL; record 6 SOL loss -> kill switch.
        let r = rm.record_loss(6_000_000_000);
        assert!(r.is_err());
        assert!(rm.is_kill_switch_active());
    }

    #[test]
    fn audit_log_written_on_kill_switch() {
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("unit test");
        let path = std::env::temp_dir().join("audit").join("risk_audit.jsonl");
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("kill_switch_triggered"));
        assert!(content.contains("unit test"));
    }
}
