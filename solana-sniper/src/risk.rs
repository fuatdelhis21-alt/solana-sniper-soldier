//! Risk management: circuit breaker, max trade size, slippage guard, daily loss limit.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
    pub circuit_breaker_duration: Duration,
    data_dir: PathBuf,
}

impl RiskConfig {
    pub fn devnet_defaults(data_dir: PathBuf) -> Self {
        Self {
            max_trade_size_lamports: 10_000_000_000,   // 10 SOL
            max_slippage_bps: 100,                     // 1%
            daily_loss_limit_lamports: 50_000_000_000, // 50 SOL
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

/// Risk manager with circuit breaker.
pub struct RiskManager {
    config: RiskConfig,
    daily_loss: Mutex<DailyLoss>,
    circuit_breaker_active: Mutex<bool>,
    last_breaker_reset: Mutex<Instant>,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            daily_loss: Mutex::new(DailyLoss::default()),
            circuit_breaker_active: Mutex::new(false),
            last_breaker_reset: Mutex::new(Instant::now()),
        }
    }

    /// Pre-trade check: size, slippage, circuit breaker, daily loss.
    pub fn pre_trade_check(
        &self,
        trade_size_lamports: u64,
        slippage_bps: u64,
    ) -> Result<(), String> {
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
        Ok(())
    }

    /// Record a loss and potentially trigger circuit breaker.
    pub fn record_loss(&self, lamports: u64) -> Result<(), String> {
        let mut daily = self.daily_loss.lock().unwrap();
        daily.loss_lamports = daily.loss_lamports.saturating_add(lamports);
        if daily.loss_lamports > self.config.daily_loss_limit_lamports {
            let mut breaker = self.circuit_breaker_active.lock().unwrap();
            *breaker = true;
            let mut reset = self.last_breaker_reset.lock().unwrap();
            *reset = Instant::now();
            return Err("circuit breaker triggered".into());
        }
        Ok(())
    }

    pub fn current_daily_loss(&self) -> u64 {
        self.daily_loss.lock().unwrap().loss_lamports
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
