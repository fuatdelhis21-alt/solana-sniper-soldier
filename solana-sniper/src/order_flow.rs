//! # Order Flow Analysis Module
//!
//! Analyzes transaction flow to detect:
//! - Whale buys (large single transactions)
//! - Sustained buying pressure (multiple buys in sequence)
//! - Liquidity removal (large sells)
//! - Bot activity (rapid-fire transactions)
//!
//! Uses VWAP (Volume-Weighted Average Price) to detect price manipulation.

use std::collections::VecDeque;

/// A transaction in the order flow.
#[derive(Debug, Clone, Copy)]
pub struct OrderFlowTx {
    pub is_buy: bool,
    pub size_lamports: u64,
    pub price: f64,
    pub timestamp_ms: u64,
}

/// Order flow analyzer.
pub struct OrderFlowAnalyzer {
    /// Last 100 transactions.
    transactions: VecDeque<OrderFlowTx>,
    /// VWAP (Volume-Weighted Average Price).
    vwap: f64,
    /// Total volume in last 60 seconds.
    recent_volume: u64,
}

impl OrderFlowAnalyzer {
    pub fn new() -> Self {
        Self {
            transactions: VecDeque::with_capacity(100),
            vwap: 0.0,
            recent_volume: 0,
        }
    }

    /// Add a transaction and update metrics.
    pub fn add_transaction(&mut self, tx: OrderFlowTx) {
        self.transactions.push_back(tx);
        while self.transactions.len() > 100 {
            self.transactions.pop_front();
        }

        // Update VWAP.
        self.update_vwap();

        // Update recent volume (last 60 seconds).
        self.recent_volume = self
            .transactions
            .iter()
            .filter(|t| tx.timestamp_ms - t.timestamp_ms <= 60_000)
            .map(|t| t.size_lamports)
            .sum();
    }

    /// Calculate VWAP.
    fn update_vwap(&mut self) {
        if self.transactions.is_empty() {
            self.vwap = 0.0;
            return;
        }

        let total_volume: u64 = self.transactions.iter().map(|t| t.size_lamports).sum();
        if total_volume == 0 {
            self.vwap = 0.0;
            return;
        }

        let weighted_price: f64 = self
            .transactions
            .iter()
            .map(|t| t.price * (t.size_lamports as f64))
            .sum::<f64>()
            / (total_volume as f64);

        self.vwap = weighted_price;
    }

    /// Detect whale buy (single transaction > 10% of recent volume).
    pub fn detect_whale_buy(&self) -> Option<u64> {
        if self.recent_volume == 0 {
            return None;
        }

        self.transactions
            .iter()
            .rev()
            .find(|t| t.is_buy && t.size_lamports > self.recent_volume / 10)
            .map(|t| t.size_lamports)
    }

    /// Detect sustained buying pressure (>60% of last 10 txs are buys).
    pub fn detect_buying_pressure(&self) -> bool {
        if self.transactions.len() < 10 {
            return false;
        }

        let last_10: Vec<_> = self.transactions.iter().rev().take(10).collect();
        let buy_count = last_10.iter().filter(|t| t.is_buy).count();
        buy_count >= 6
    }

    /// Detect liquidity removal (large sell).
    pub fn detect_liquidity_removal(&self) -> Option<u64> {
        if self.recent_volume == 0 {
            return None;
        }

        self.transactions
            .iter()
            .rev()
            .find(|t| !t.is_buy && t.size_lamports > self.recent_volume / 10)
            .map(|t| t.size_lamports)
    }

    /// Detect bot activity (rapid-fire transactions, >5 txs in 5 seconds).
    pub fn detect_bot_activity(&self) -> bool {
        if self.transactions.is_empty() {
            return false;
        }

        let latest = self.transactions.back().unwrap();
        let rapid_txs = self
            .transactions
            .iter()
            .filter(|t| latest.timestamp_ms - t.timestamp_ms <= 5_000)
            .count();

        rapid_txs > 5
    }

    /// Get buy/sell ratio (last 20 txs).
    pub fn buy_sell_ratio(&self) -> f64 {
        if self.transactions.is_empty() {
            return 1.0;
        }

        let last_20: Vec<_> = self.transactions.iter().rev().take(20).collect();
        let buy_count = last_20.iter().filter(|t| t.is_buy).count() as f64;
        let sell_count = last_20.iter().filter(|t| !t.is_buy).count() as f64;

        if sell_count == 0.0 {
            buy_count
        } else {
            buy_count / sell_count
        }
    }

    /// Get current VWAP.
    pub fn get_vwap(&self) -> f64 {
        self.vwap
    }

    /// Get recent volume (last 60 seconds).
    pub fn get_recent_volume(&self) -> u64 {
        self.recent_volume
    }

    /// Check if price is above VWAP (bullish signal).
    pub fn is_price_above_vwap(&self, current_price: f64) -> bool {
        self.vwap > 0.0 && current_price > self.vwap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whale_buy_detection() {
        let mut analyzer = OrderFlowAnalyzer::new();
        let now = 1000u64;

        // Add baseline transactions (100 lamports each).
        for i in 0..10 {
            analyzer.add_transaction(OrderFlowTx {
                is_buy: true,
                size_lamports: 100,
                price: 1.0,
                timestamp_ms: now + (i as u64 * 1000),
            });
        }

        // Add whale buy (1500 lamports > 10% of 1000).
        analyzer.add_transaction(OrderFlowTx {
            is_buy: true,
            size_lamports: 1500,
            price: 1.0,
            timestamp_ms: now + 10000,
        });

        assert!(analyzer.detect_whale_buy().is_some());
    }

    #[test]
    fn test_buying_pressure() {
        let mut analyzer = OrderFlowAnalyzer::new();
        let now = 1000u64;

        // Add 8 buys and 2 sells.
        for i in 0..8 {
            analyzer.add_transaction(OrderFlowTx {
                is_buy: true,
                size_lamports: 100,
                price: 1.0,
                timestamp_ms: now + (i as u64 * 1000),
            });
        }
        for i in 0..2 {
            analyzer.add_transaction(OrderFlowTx {
                is_buy: false,
                size_lamports: 100,
                price: 1.0,
                timestamp_ms: now + (8000 + i as u64 * 1000),
            });
        }

        assert!(analyzer.detect_buying_pressure());
    }

    #[test]
    fn test_vwap_calculation() {
        let mut analyzer = OrderFlowAnalyzer::new();

        // Buy 100 at price 1.0, buy 100 at price 2.0.
        analyzer.add_transaction(OrderFlowTx {
            is_buy: true,
            size_lamports: 100,
            price: 1.0,
            timestamp_ms: 1000,
        });
        analyzer.add_transaction(OrderFlowTx {
            is_buy: true,
            size_lamports: 100,
            price: 2.0,
            timestamp_ms: 2000,
        });

        let vwap = analyzer.get_vwap();
        assert!((vwap - 1.5).abs() < 0.01, "VWAP should be 1.5: {}", vwap);
    }
}
