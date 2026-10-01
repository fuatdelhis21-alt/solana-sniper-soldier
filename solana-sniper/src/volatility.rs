//! # Volatility Module
//!
//! Calculates realized volatility and adjusts TP/SL dynamically.
//! - High volatility → wider TP/SL (capture more upside, avoid whipsaws)
//! - Low volatility → tighter TP/SL (quick profits, less risk)
//!
//! Uses exponential moving average (EMA) for smooth volatility tracking.

use std::collections::VecDeque;

/// Volatility calculator using EMA of returns.
pub struct VolatilityCalculator {
    /// Last 60 price points (1 per second).
    prices: VecDeque<f64>,
    /// EMA of squared returns (variance proxy).
    ema_variance: f64,
    /// EMA smoothing factor (0.1 = 10% weight to new data).
    alpha: f64,
}

impl VolatilityCalculator {
    pub fn new() -> Self {
        Self {
            prices: VecDeque::with_capacity(60),
            ema_variance: 0.0,
            alpha: 0.1,
        }
    }

    /// Add a new price point and update volatility.
    pub fn update(&mut self, price: f64) {
        self.prices.push_back(price);
        while self.prices.len() > 60 {
            self.prices.pop_front();
        }

        if self.prices.len() >= 2 {
            let prev_price = self.prices[self.prices.len() - 2];
            let return_pct = (price - prev_price) / prev_price;
            let squared_return = return_pct * return_pct;

            // Update EMA of variance.
            if self.ema_variance == 0.0 {
                self.ema_variance = squared_return;
            } else {
                self.ema_variance = self.alpha * squared_return + (1.0 - self.alpha) * self.ema_variance;
            }
        }
    }

    /// Get realized volatility (annualized, in percentage).
    pub fn volatility_pct(&self) -> f64 {
        let variance = self.ema_variance;
        let std_dev = variance.sqrt();
        // Annualize: sqrt(252 trading days) ≈ 15.87, but for intraday use sqrt(86400 seconds / 1 second) ≈ 294
        // For simplicity, use sqrt(252) for daily-equivalent volatility.
        std_dev * 15.87 * 100.0
    }

    /// Get dynamic take-profit threshold (bps) based on volatility.
    /// Low vol (5%) → 500 bps (5%), High vol (50%) → 2000 bps (20%).
    pub fn dynamic_take_profit_bps(&self) -> u64 {
        let vol = self.volatility_pct().min(100.0).max(1.0);
        // Linear scaling: vol 5% → 500 bps, vol 50% → 2000 bps.
        let base_bps = 500u64;
        let vol_factor = (vol - 5.0) / 45.0; // Normalize to 0-1 for vol 5-50%.
        let additional_bps = (vol_factor * 1500.0) as u64;
        base_bps + additional_bps
    }

    /// Get dynamic stop-loss threshold (bps) based on volatility.
    /// Low vol (5%) → 250 bps (2.5%), High vol (50%) → 1000 bps (10%).
    pub fn dynamic_stop_loss_bps(&self) -> u64 {
        let vol = self.volatility_pct().min(100.0).max(1.0);
        let base_bps = 250u64;
        let vol_factor = (vol - 5.0) / 45.0;
        let additional_bps = (vol_factor * 750.0) as u64;
        base_bps + additional_bps
    }

    /// Get position size adjustment factor based on volatility.
    /// High vol → smaller position (0.5x), Low vol → larger position (1.5x).
    pub fn position_size_factor(&self) -> f64 {
        let vol = self.volatility_pct().min(100.0).max(1.0);
        // vol 5% → 1.5x, vol 50% → 0.5x.
        let vol_factor = (vol - 5.0) / 45.0;
        1.5 - (vol_factor * 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volatility_calculation() {
        let mut calc = VolatilityCalculator::new();

        // Stable prices: 100, 100.5, 100.5, 100.5 (low volatility).
        for price in [100.0, 100.5, 100.5, 100.5].iter() {
            calc.update(*price);
        }
        let low_vol = calc.volatility_pct();
        assert!(low_vol < 10.0, "stable prices should have low volatility: {}", low_vol);

        // Reset and test high volatility.
        let mut calc = VolatilityCalculator::new();
        for price in [100.0, 105.0, 95.0, 110.0, 90.0].iter() {
            calc.update(*price);
        }
        let high_vol = calc.volatility_pct();
        assert!(high_vol > 20.0, "volatile prices should have high volatility: {}", high_vol);
    }

    #[test]
    fn test_dynamic_tp_sl() {
        let mut calc = VolatilityCalculator::new();

        // Low volatility scenario.
        for price in [100.0, 100.1, 100.2, 100.1, 100.2].iter() {
            calc.update(*price);
        }
        let tp_low = calc.dynamic_take_profit_bps();
        let sl_low = calc.dynamic_stop_loss_bps();
        assert!(tp_low < 1000, "low vol should have tight TP: {}", tp_low);
        assert!(sl_low < 500, "low vol should have tight SL: {}", sl_low);

        // High volatility scenario.
        let mut calc = VolatilityCalculator::new();
        for price in [100.0, 110.0, 90.0, 120.0, 80.0].iter() {
            calc.update(*price);
        }
        let tp_high = calc.dynamic_take_profit_bps();
        let sl_high = calc.dynamic_stop_loss_bps();
        assert!(tp_high > 1000, "high vol should have wide TP: {}", tp_high);
        assert!(sl_high > 500, "high vol should have wide SL: {}", sl_high);
    }

    #[test]
    fn test_position_size_factor() {
        let mut calc = VolatilityCalculator::new();

        // Low volatility → larger position.
        for price in [100.0, 100.1, 100.2, 100.1, 100.2].iter() {
            calc.update(*price);
        }
        let factor_low = calc.position_size_factor();
        assert!(factor_low > 1.0, "low vol should increase position size: {}", factor_low);

        // High volatility → smaller position.
        let mut calc = VolatilityCalculator::new();
        for price in [100.0, 110.0, 90.0, 120.0, 80.0].iter() {
            calc.update(*price);
        }
        let factor_high = calc.position_size_factor();
        assert!(factor_high < 1.0, "high vol should decrease position size: {}", factor_high);
    }
}
