//! # Position Sizing Module
//!
//! Calculates optimal position size using:
//! - Kelly Criterion (maximize long-term growth)
//! - Risk-adjusted sizing (volatility, win rate, R:R ratio)
//! - Portfolio heat (total exposure across all positions)
//! - Drawdown protection (never risk more than daily loss limit)
//!
//! Formula: Position Size = (Win% * AvgWin - Loss% * AvgLoss) / AvgWin
//! Adjusted for volatility and portfolio constraints.

/// Position sizing calculator.
pub struct PositionSizer {
    /// Win rate (0-1).
    pub win_rate: f64,
    /// Average win size (bps).
    pub avg_win_bps: u64,
    /// Average loss size (bps).
    pub avg_loss_bps: u64,
    /// Total portfolio value (lamports).
    pub portfolio_value: u64,
    /// Current portfolio heat (lamports already at risk).
    pub current_heat: u64,
    /// Maximum daily loss limit (lamports).
    pub max_daily_loss: u64,
}

impl PositionSizer {
    pub fn new(
        portfolio_value: u64,
        max_daily_loss: u64,
    ) -> Self {
        Self {
            win_rate: 0.55,           // Conservative: 55% win rate.
            avg_win_bps: 1000,        // 10% average win.
            avg_loss_bps: 500,        // 5% average loss.
            portfolio_value,
            current_heat: 0,
            max_daily_loss,
        }
    }

    /// Update win/loss statistics from trade history.
    pub fn update_stats(&mut self, win_rate: f64, avg_win_bps: u64, avg_loss_bps: u64) {
        self.win_rate = win_rate.max(0.0).min(1.0);
        self.avg_win_bps = avg_win_bps.max(1);
        self.avg_loss_bps = avg_loss_bps.max(1);
    }

    /// Calculate Kelly Criterion position size (as % of portfolio).
    /// Formula: f* = (p * b - q) / b
    /// where p = win rate, q = loss rate, b = win/loss ratio.
    pub fn kelly_fraction(&self) -> f64 {
        let p = self.win_rate;
        let q = 1.0 - p;
        let b = (self.avg_win_bps as f64) / (self.avg_loss_bps as f64);

        let kelly = (p * b - q) / b;
        // Fractional Kelly (use 25% of Kelly to reduce volatility).
        (kelly * 0.25).max(0.0).min(0.1) // Cap at 10% of portfolio.
    }

    /// Calculate optimal position size (lamports) for a trade.
    ///
    /// Inputs:
    /// - `volatility_pct`: Current market volatility (%)
    /// - `pump_score`: Pump detection score (0-100)
    /// - `liquidity_lamports`: Pool liquidity
    ///
    /// Returns: Recommended position size (lamports).
    pub fn optimal_position_size(
        &self,
        volatility_pct: f64,
        pump_score: u32,
        liquidity_lamports: u64,
    ) -> u64 {
        // Base size from Kelly Criterion.
        let kelly_fraction = self.kelly_fraction();
        let base_size = (self.portfolio_value as f64 * kelly_fraction) as u64;

        // Volatility adjustment: high vol → smaller position.
        let vol_factor = 1.0 / (1.0 + volatility_pct / 50.0);
        let vol_adjusted = (base_size as f64 * vol_factor) as u64;

        // Pump score adjustment: high pump score → larger position (more confident).
        let pump_factor = 0.5 + (pump_score as f64 / 100.0) * 0.5; // 0.5x to 1.5x.
        let pump_adjusted = (vol_adjusted as f64 * pump_factor) as u64;

        // Liquidity constraint: never trade more than 5% of pool.
        let max_liquidity_size = liquidity_lamports / 20;
        let liquidity_constrained = pump_adjusted.min(max_liquidity_size);

        // Heat constraint: never exceed remaining daily loss budget.
        let remaining_budget = self.max_daily_loss.saturating_sub(self.current_heat);
        let heat_constrained = liquidity_constrained.min(remaining_budget);

        heat_constrained
    }

    /// Update current portfolio heat (total at-risk lamports).
    pub fn update_heat(&mut self, position_size: u64) {
        self.current_heat = self.current_heat.saturating_add(position_size);
    }

    /// Close a position and reduce heat.
    pub fn close_position(&mut self, position_size: u64) {
        self.current_heat = self.current_heat.saturating_sub(position_size);
    }

    /// Get remaining daily loss budget (lamports).
    pub fn remaining_budget(&self) -> u64 {
        self.max_daily_loss.saturating_sub(self.current_heat)
    }

    /// Check if we can open a new position of given size.
    pub fn can_open_position(&self, size: u64) -> bool {
        self.current_heat + size <= self.max_daily_loss
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kelly_criterion() {
        let sizer = PositionSizer::new(1_000_000_000, 50_000_000); // 1 SOL portfolio, 0.05 SOL max loss.
        let kelly = sizer.kelly_fraction();
        assert!(kelly > 0.0 && kelly <= 0.1, "kelly should be between 0 and 10%: {}", kelly);
    }

    #[test]
    fn test_position_sizing_volatility() {
        let sizer = PositionSizer::new(1_000_000_000, 50_000_000);

        // Low volatility → larger position.
        let size_low_vol = sizer.optimal_position_size(5.0, 50, 100_000_000_000);

        // High volatility → smaller position.
        let size_high_vol = sizer.optimal_position_size(50.0, 50, 100_000_000_000);

        assert!(size_low_vol > size_high_vol, "low vol should allow larger position");
    }

    #[test]
    fn test_position_sizing_pump_score() {
        let sizer = PositionSizer::new(1_000_000_000, 50_000_000);

        // Low pump score → smaller position.
        let size_low_pump = sizer.optimal_position_size(20.0, 20, 100_000_000_000);

        // High pump score → larger position.
        let size_high_pump = sizer.optimal_position_size(20.0, 80, 100_000_000_000);

        assert!(size_high_pump > size_low_pump, "high pump score should allow larger position");
    }

    #[test]
    fn test_heat_tracking() {
        let mut sizer = PositionSizer::new(1_000_000_000, 50_000_000);

        assert!(sizer.can_open_position(30_000_000));
        sizer.update_heat(30_000_000);
        assert!(sizer.can_open_position(20_000_000));
        assert!(!sizer.can_open_position(30_000_000));

        sizer.close_position(30_000_000);
        assert!(sizer.can_open_position(30_000_000));
    }
}
