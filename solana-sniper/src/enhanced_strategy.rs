//! # Enhanced Strategy Module
//!
//! Integrates all HFT components:
//! - Pump detection (volume spike + momentum)
//! - Dynamic entry/exit (volatility-based TP/SL)
//! - Position sizing (Kelly Criterion + risk-adjusted)
//! - Order flow analysis (whale detection, buying pressure)
//! - Slippage optimization (dynamic tolerance)
//!
//! Produces high-confidence entry signals with optimized parameters.

use crate::order_flow::{OrderFlowAnalyzer, OrderFlowTx};
use crate::position_sizing::PositionSizer;
use crate::pump_detection::{PricePoint, PumpDetector};
use crate::slippage_optimizer::SlippageOptimizer;
use crate::volatility::VolatilityCalculator;
use serde::{Deserialize, Serialize};

/// Enhanced entry signal with all optimized parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnhancedEntrySignal {
    /// Position size (lamports) — Kelly Criterion + volatility-adjusted.
    pub position_size_lamports: u64,
    /// Dynamic take-profit (bps) — volatility-based.
    pub take_profit_bps: u64,
    /// Dynamic stop-loss (bps) — volatility-based.
    pub stop_loss_bps: u64,
    /// Slippage tolerance (bps) — pool liquidity + trade size adjusted.
    pub slippage_bps: u64,
    /// Pump detection score (0-100).
    pub pump_score: u32,
    /// Confidence level (0-100).
    pub confidence: u32,
    /// Entry price (Q64.64 sqrt price).
    pub entry_sqrt_price: u128,
}

/// Enhanced strategy combining all HFT components.
pub struct EnhancedStrategy {
    pump_detector: PumpDetector,
    volatility_calc: VolatilityCalculator,
    position_sizer: PositionSizer,
    slippage_optimizer: SlippageOptimizer,
    order_flow_analyzer: OrderFlowAnalyzer,
}

impl EnhancedStrategy {
    pub fn new(portfolio_value: u64, max_daily_loss: u64) -> Self {
        Self {
            pump_detector: PumpDetector::new(),
            volatility_calc: VolatilityCalculator::new(),
            position_sizer: PositionSizer::new(portfolio_value, max_daily_loss),
            slippage_optimizer: SlippageOptimizer::new(),
            order_flow_analyzer: OrderFlowAnalyzer::new(),
        }
    }

    /// Update market data and check for entry signal.
    pub fn update_market_data(
        &mut self,
        price_point: PricePoint,
        order_flow_tx: Option<OrderFlowTx>,
    ) -> Option<EnhancedEntrySignal> {
        // Update pump detector.
        let pump_detected = self.pump_detector.update(price_point);
        let pump_score = self.pump_detector.pump_score();

        // Update volatility calculator.
        let price = sqrt_price_to_price(price_point.sqrt_price_x64);
        self.volatility_calc.update(price);

        // Update order flow analyzer.
        if let Some(tx) = order_flow_tx {
            self.order_flow_analyzer.add_transaction(tx);
        }

        // Check entry conditions.
        if !pump_detected {
            return None;
        }

        // Verify with order flow signals.
        let buying_pressure = self.order_flow_analyzer.detect_buying_pressure();
        let whale_buy = self.order_flow_analyzer.detect_whale_buy().is_some();
        let price_above_vwap = self
            .order_flow_analyzer
            .is_price_above_vwap(price);

        // Require at least 2 of 3 signals.
        let signal_count = [buying_pressure, whale_buy, price_above_vwap]
            .iter()
            .filter(|&&x| x)
            .count();
        if signal_count < 2 {
            return None;
        }

        // Calculate dynamic parameters.
        let volatility_pct = self.volatility_calc.volatility_pct();
        let take_profit_bps = self.volatility_calc.dynamic_take_profit_bps();
        let stop_loss_bps = self.volatility_calc.dynamic_stop_loss_bps();

        // Calculate position size.
        let position_size = self.position_sizer.optimal_position_size(
            volatility_pct,
            pump_score,
            1_000_000_000_000, // Assume 1000 SOL pool (will be overridden by actual).
        );

        // Calculate slippage tolerance.
        let slippage_bps = self.slippage_optimizer.optimal_slippage_bps(
            1_000_000_000_000,
            position_size,
            volatility_pct,
            50, // Base 50 bps.
        );

        // Calculate confidence (0-100).
        let mut confidence = pump_score as u32;
        if buying_pressure {
            confidence += 10;
        }
        if whale_buy {
            confidence += 10;
        }
        if price_above_vwap {
            confidence += 5;
        }
        confidence = confidence.min(100);

        Some(EnhancedEntrySignal {
            position_size_lamports: position_size,
            take_profit_bps,
            stop_loss_bps,
            slippage_bps,
            pump_score,
            confidence,
            entry_sqrt_price: price_point.sqrt_price_x64,
        })
    }

    /// Record a completed trade for learning.
    pub fn record_trade(
        &mut self,
        expected_output: u64,
        actual_output: u64,
        position_size: u64,
    ) {
        use crate::slippage_optimizer::SlippageRecord;

        let slippage_bps = if expected_output > 0 {
            ((expected_output - actual_output) as f64 / expected_output as f64 * 10_000.0) as u64
        } else {
            0
        };

        self.slippage_optimizer.record_trade(SlippageRecord {
            expected_output,
            actual_output,
            slippage_bps,
        });

        self.position_sizer.update_heat(position_size);
    }

    /// Reset detectors after entry.
    pub fn reset_detectors(&mut self) {
        self.pump_detector.reset();
    }

    /// Get current metrics for monitoring.
    pub fn get_metrics(&self) -> StrategyMetrics {
        StrategyMetrics {
            pump_score: self.pump_detector.pump_score(),
            volatility_pct: self.volatility_calc.volatility_pct(),
            buy_sell_ratio: self.order_flow_analyzer.buy_sell_ratio(),
            vwap: self.order_flow_analyzer.get_vwap(),
            recent_volume: self.order_flow_analyzer.get_recent_volume(),
            current_heat: self.position_sizer.current_heat,
            remaining_budget: self.position_sizer.remaining_budget(),
        }
    }
}

/// Strategy metrics for monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyMetrics {
    pub pump_score: u32,
    pub volatility_pct: f64,
    pub buy_sell_ratio: f64,
    pub vwap: f64,
    pub recent_volume: u64,
    pub current_heat: u64,
    pub remaining_budget: u64,
}

/// Convert Q64.64 sqrt price to decimal price.
fn sqrt_price_to_price(sqrt_price_x64: u128) -> f64 {
    let sqrt_price_f64 = (sqrt_price_x64 as f64) / ((1u128 << 64) as f64);
    sqrt_price_f64 * sqrt_price_f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enhanced_entry_signal() {
        let mut strategy = EnhancedStrategy::new(1_000_000_000, 50_000_000);

        // Simulate pump: volume spike + price momentum.
        let now = 1000u64;
        for i in 0..60 {
            let price_point = PricePoint {
                sqrt_price_x64: 1u128 << 64,
                volume_lamports: 100,
                timestamp_ms: now + (i as u64 * 1000),
            };
            strategy.update_market_data(price_point, None);
        }

        // Trigger pump with volume spike + price move.
        let spike_price = ((1u128 << 64) as f64 * 1.05) as u128;
        let mut signal = None;
        for i in 0..30 {
            let price_point = PricePoint {
                sqrt_price_x64: spike_price,
                volume_lamports: 600,
                timestamp_ms: now + (60000 + i as u64 * 1000),
            };

            // Add buying pressure signal.
            let order_flow = Some(OrderFlowTx {
                is_buy: true,
                size_lamports: 100,
                price: 1.05,
                timestamp_ms: now + (60000 + i as u64 * 1000),
            });

            signal = strategy.update_market_data(price_point, order_flow);
            if signal.is_some() {
                break;
            }
        }

        assert!(signal.is_some(), "should generate entry signal on pump");
        let sig = signal.unwrap();
        assert!(sig.pump_score > 50, "pump score should be high");
        assert!(sig.confidence > 50, "confidence should be high");
        assert!(sig.position_size_lamports > 0, "position size should be positive");
    }
}
