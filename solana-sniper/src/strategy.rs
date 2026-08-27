//! # Strategy Module — token filtering + entry/exit criteria
//!
//! Pure, deterministic strategy logic. No on-chain I/O here: this module only
//! decides *whether* to trade and *when to exit*. Execution is handled by the
//! executor / AMM adapters.
//!
//! ## Safety
//! - All limits are conservative, fail-closed defaults (see `StrategyConfig`).
//! - Capital preservation always outranks trade frequency or profitability.
//! - A token that fails any filter is rejected outright (no partial entry).

use serde::{Deserialize, Serialize};

/// Strategy configuration with conservative, fail-closed defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Minimum pool liquidity (lamports) required to consider a token.
    pub min_liquidity_lamports: u64,
    /// Maximum position size per trade (lamports).
    pub max_trade_size_lamports: u64,
    /// Maximum acceptable slippage (basis points, 1% = 100).
    pub max_slippage_bps: u64,
    /// Stop-loss threshold (basis points below entry price).
    pub stop_loss_bps: u64,
    /// Take-profit threshold (basis points above entry price).
    pub take_profit_bps: u64,
    /// Maximum number of trades per day (rolling).
    pub max_daily_trades: u64,
    /// Maximum market cap (lamports) — reject overvalued / already-pumped tokens.
    pub max_market_cap_lamports: u64,
    /// Minimum number of holders required.
    pub min_holders: u64,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            // 1000 SOL liquidity floor (conservative for devnet testing).
            min_liquidity_lamports: 1_000_000_000_000,
            // 0.1 SOL max position per trade.
            max_trade_size_lamports: 100_000_000,
            // 1% max slippage.
            max_slippage_bps: 100,
            // 5% stop-loss.
            stop_loss_bps: 500,
            // 10% take-profit.
            take_profit_bps: 1_000,
            // 20 trades/day cap.
            max_daily_trades: 20,
            // 1M SOL market cap ceiling.
            max_market_cap_lamports: 1_000_000_000_000_000,
            // Minimum 50 holders.
            min_holders: 50,
        }
    }
}

/// A token candidate being evaluated for entry.
#[derive(Debug, Clone)]
pub struct TokenCandidate {
    /// Pool liquidity in lamports.
    pub liquidity_lamports: u64,
    /// Market cap in lamports.
    pub market_cap_lamports: u64,
    /// Number of holders.
    pub holders: u64,
    /// Whether the mint is on a known rug-pull / honeypot blocklist.
    pub is_blocklisted: bool,
}

/// Entry signal produced when a candidate passes all filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySignal {
    /// Position size to enter (lamports).
    pub position_size_lamports: u64,
    /// Maximum slippage allowed for this entry (bps).
    pub slippage_bps: u64,
    /// Entry price reference (Q64.64 sqrt price) for stop-loss/take-profit.
    pub entry_sqrt_price: u128,
}

/// Exit decision for an open position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitDecision {
    /// Hold the position.
    Hold,
    /// Exit because price fell to the stop-loss threshold.
    StopLoss,
    /// Exit because price reached the take-profit threshold.
    TakeProfit,
}

/// Pure strategy logic. Deterministic — no I/O, no randomness.
pub struct SimpleSnipeStrategy {
    config: StrategyConfig,
}

impl SimpleSnipeStrategy {
    pub fn new(config: StrategyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &StrategyConfig {
        &self.config
    }

    /// Evaluate a token candidate for entry. Returns `Some(EntrySignal)` only
    /// if the candidate passes *every* filter (fail-closed).
    pub fn evaluate(
        &self,
        candidate: &TokenCandidate,
        entry_sqrt_price: u128,
    ) -> Option<EntrySignal> {
        // Fail-closed: any disqualifying condition rejects the token.
        if candidate.is_blocklisted {
            return None;
        }
        if candidate.liquidity_lamports < self.config.min_liquidity_lamports {
            return None;
        }
        if candidate.market_cap_lamports > self.config.max_market_cap_lamports {
            return None;
        }
        if candidate.holders < self.config.min_holders {
            return None;
        }

        Some(EntrySignal {
            position_size_lamports: self.config.max_trade_size_lamports,
            slippage_bps: self.config.max_slippage_bps,
            entry_sqrt_price,
        })
    }

    /// Decide whether to exit an open position given the current price.
    ///
    /// `current_sqrt_price` and `entry_sqrt_price` are Q64.64 fixed-point.
    /// Price ratio = (current/entry)^2. We compare the squared ratio against
    /// the bps thresholds. f64 is used for the threshold comparison; the
    /// magnitudes involved (Q64.64 sqrt prices) are far from f64 precision
    /// limits, so this is exact enough for stop-loss/take-profit bands.
    pub fn should_exit(&self, entry_sqrt_price: u128, current_sqrt_price: u128) -> ExitDecision {
        if entry_sqrt_price == 0 {
            return ExitDecision::Hold;
        }
        let entry = entry_sqrt_price as f64;
        let current = current_sqrt_price as f64;
        // price_ratio = (current/entry)^2
        let price_ratio = (current / entry) * (current / entry);
        if price_ratio >= 1.0 {
            let profit_bps = (price_ratio - 1.0) * 10_000.0;
            if profit_bps >= self.config.take_profit_bps as f64 {
                return ExitDecision::TakeProfit;
            }
        } else {
            let loss_bps = (1.0 - price_ratio) * 10_000.0;
            if loss_bps >= self.config.stop_loss_bps as f64 {
                return ExitDecision::StopLoss;
            }
        }
        ExitDecision::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_candidate() -> TokenCandidate {
        TokenCandidate {
            liquidity_lamports: 2_000_000_000_000,
            market_cap_lamports: 100_000_000_000_000,
            holders: 200,
            is_blocklisted: false,
        }
    }

    #[test]
    fn evaluate_accepts_valid_candidate() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let sig = s.evaluate(&valid_candidate(), 1u128 << 64).unwrap();
        assert_eq!(sig.position_size_lamports, 100_000_000);
        assert_eq!(sig.slippage_bps, 100);
    }

    #[test]
    fn evaluate_rejects_blocklisted() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let mut c = valid_candidate();
        c.is_blocklisted = true;
        assert!(s.evaluate(&c, 1u128 << 64).is_none());
    }

    #[test]
    fn evaluate_rejects_low_liquidity() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let mut c = valid_candidate();
        c.liquidity_lamports = 100_000_000; // below 1000 SOL floor
        assert!(s.evaluate(&c, 1u128 << 64).is_none());
    }

    #[test]
    fn evaluate_rejects_high_market_cap() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let mut c = valid_candidate();
        c.market_cap_lamports = 9_000_000_000_000_000; // above 1M SOL ceiling
        assert!(s.evaluate(&c, 1u128 << 64).is_none());
    }

    #[test]
    fn evaluate_rejects_few_holders() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let mut c = valid_candidate();
        c.holders = 10;
        assert!(s.evaluate(&c, 1u128 << 64).is_none());
    }

    #[test]
    fn should_exit_holds_at_entry() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let entry = 1u128 << 64;
        assert_eq!(s.should_exit(entry, entry), ExitDecision::Hold);
    }

    #[test]
    fn should_exit_take_profit_at_10pct() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let entry = 1u128 << 64;
        // 10% price increase => sqrt_price * sqrt(1.10)
        let sqrt_110 = (1.10f64).sqrt();
        let current = ((entry as f64) * sqrt_110) as u128;
        assert_eq!(s.should_exit(entry, current), ExitDecision::TakeProfit);
    }

    #[test]
    fn should_exit_stop_loss_at_5pct() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let entry = 1u128 << 64;
        // 5% price decrease => sqrt_price * sqrt(0.95)
        let sqrt_095 = (0.95f64).sqrt();
        let current = ((entry as f64) * sqrt_095) as u128;
        assert_eq!(s.should_exit(entry, current), ExitDecision::StopLoss);
    }

    #[test]
    fn should_exit_holds_within_band() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let entry = 1u128 << 64;
        // 2% up — below take-profit, above stop-loss.
        let current = ((entry as f64) * (1.02f64).sqrt()) as u128;
        assert_eq!(s.should_exit(entry, current), ExitDecision::Hold);
    }

    #[test]
    fn should_exit_holds_on_zero_entry() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        assert_eq!(s.should_exit(0, 1u128 << 64), ExitDecision::Hold);
    }
}
