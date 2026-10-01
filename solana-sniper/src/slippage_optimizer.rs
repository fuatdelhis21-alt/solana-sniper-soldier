//! # Slippage Optimizer
//!
//! Dynamically adjusts slippage tolerance based on:
//! - Pool liquidity (deeper pools = lower slippage)
//! - Trade size relative to pool (larger trades = higher slippage)
//! - Market volatility (volatile markets = higher slippage)
//! - Historical slippage (track actual vs expected)
//!
//! Goal: Maximize execution probability while minimizing slippage cost.

use std::collections::VecDeque;

/// Historical slippage record.
#[derive(Debug, Clone, Copy)]
pub struct SlippageRecord {
    pub expected_output: u64,
    pub actual_output: u64,
    pub slippage_bps: u64,
}

/// Slippage optimizer.
pub struct SlippageOptimizer {
    /// Last 20 trades' slippage records.
    history: VecDeque<SlippageRecord>,
    /// Average slippage over last 20 trades (bps).
    avg_slippage_bps: u64,
}

impl SlippageOptimizer {
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(20),
            avg_slippage_bps: 50, // Start conservative.
        }
    }

    /// Record a completed trade's actual slippage.
    pub fn record_trade(&mut self, record: SlippageRecord) {
        self.history.push_back(record);
        while self.history.len() > 20 {
            self.history.pop_front();
        }

        // Update average.
        self.avg_slippage_bps = if self.history.is_empty() {
            50
        } else {
            self.history.iter().map(|r| r.slippage_bps).sum::<u64>() / self.history.len() as u64
        };
    }

    /// Calculate optimal slippage tolerance (bps) for a trade.
    ///
    /// Inputs:
    /// - `pool_liquidity_lamports`: Total pool liquidity
    /// - `trade_size_lamports`: Size of this trade
    /// - `volatility_pct`: Current market volatility (%)
    /// - `base_slippage_bps`: Minimum acceptable slippage
    ///
    /// Returns: Recommended slippage tolerance (bps).
    pub fn optimal_slippage_bps(
        &self,
        pool_liquidity_lamports: u64,
        trade_size_lamports: u64,
        volatility_pct: f64,
        base_slippage_bps: u64,
    ) -> u64 {
        // Factor 1: Trade size relative to pool (0-100 bps).
        let size_ratio = (trade_size_lamports as f64) / (pool_liquidity_lamports as f64);
        let size_slippage = std::cmp::min(100, (size_ratio * 1000.0) as u64);

        // Factor 2: Volatility (0-100 bps).
        let vol_slippage = std::cmp::min(100, (volatility_pct * 2.0) as u64);

        // Factor 3: Historical slippage (0-50 bps).
        let hist_slippage = std::cmp::min(50, self.avg_slippage_bps / 2);

        // Combine: base + size + volatility + history.
        base_slippage_bps + size_slippage + vol_slippage + hist_slippage
    }

    /// Check if actual slippage is acceptable (within tolerance).
    pub fn is_acceptable_slippage(
        &self,
        expected_output: u64,
        actual_output: u64,
        tolerance_bps: u64,
    ) -> bool {
        if expected_output == 0 {
            return false;
        }

        let slippage_bps = ((expected_output - actual_output) as f64 / expected_output as f64
            * 10_000.0) as u64;
        slippage_bps <= tolerance_bps
    }

    /// Get average historical slippage (bps).
    pub fn average_slippage_bps(&self) -> u64 {
        self.avg_slippage_bps
    }

    /// Get slippage percentile (e.g., 95th percentile for conservative estimate).
    pub fn slippage_percentile(&self, percentile: f64) -> u64 {
        if self.history.is_empty() {
            return 50;
        }

        let mut slippages: Vec<u64> = self.history.iter().map(|r| r.slippage_bps).collect();
        slippages.sort_unstable();

        let idx = ((percentile / 100.0) * slippages.len() as f64) as usize;
        slippages[idx.min(slippages.len() - 1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimal_slippage_calculation() {
        let optimizer = SlippageOptimizer::new();

        // Small trade in deep pool, low volatility.
        let slippage = optimizer.optimal_slippage_bps(
            1_000_000_000_000, // 1000 SOL pool
            10_000_000,        // 0.01 SOL trade
            5.0,               // 5% volatility
            50,                // 50 bps base
        );
        assert!(slippage < 200, "small trade should have low slippage: {}", slippage);

        // Large trade in shallow pool, high volatility.
        let slippage = optimizer.optimal_slippage_bps(
            100_000_000_000, // 100 SOL pool
            50_000_000,      // 0.05 SOL trade (50% of pool!)
            50.0,            // 50% volatility
            50,              // 50 bps base
        );
        assert!(slippage > 150, "large trade should have higher slippage: {}", slippage);
    }

    #[test]
    fn test_slippage_acceptance() {
        let optimizer = SlippageOptimizer::new();

        // Expected 1000, got 990 (1% slippage).
        assert!(optimizer.is_acceptable_slippage(1000, 990, 150));
        assert!(!optimizer.is_acceptable_slippage(1000, 990, 50));
    }

    #[test]
    fn test_historical_tracking() {
        let mut optimizer = SlippageOptimizer::new();

        // Record 5 trades with increasing slippage.
        for i in 0..5 {
            optimizer.record_trade(SlippageRecord {
                expected_output: 1000,
                actual_output: 1000 - (i as u64 * 10),
                slippage_bps: i as u64 * 100,
            });
        }

        let avg = optimizer.average_slippage_bps();
        assert_eq!(avg, 200, "average of 0,100,200,300,400 should be 200");
    }
}
