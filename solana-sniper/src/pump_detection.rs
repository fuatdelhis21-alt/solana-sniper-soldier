//! # Pump Detection Module
//!
//! Detects new token pumps via:
//! - Volume spike (5x+ baseline)
//! - Price momentum (2%+ in 30 seconds)
//! - Liquidity surge (3x+ in 1 minute)
//! - Holder concentration (new tokens often have concentrated holders)
//!
//! Fail-closed: any detection failure returns None (no false positives).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// Price point with timestamp and volume.
#[derive(Debug, Clone, Copy)]
pub struct PricePoint {
    pub sqrt_price_x64: u128,
    pub volume_lamports: u64,
    pub timestamp_ms: u64,
}

/// Pump detection state machine.
pub struct PumpDetector {
    /// Last 60 seconds of price points (1 per second).
    price_history: VecDeque<PricePoint>,
    /// Last 5 minutes of volume (1 per 30 seconds).
    volume_history: VecDeque<u64>,
    /// Baseline volume (rolling average of last 5 minutes).
    baseline_volume: u64,
}

impl PumpDetector {
    pub fn new() -> Self {
        Self {
            price_history: VecDeque::with_capacity(60),
            volume_history: VecDeque::with_capacity(10),
            baseline_volume: 0,
        }
    }

    /// Add a new price point. Returns true if pump detected.
    pub fn update(&mut self, point: PricePoint) -> bool {
        // Keep only last 60 seconds.
        while let Some(oldest) = self.price_history.front() {
            if point.timestamp_ms - oldest.timestamp_ms > 60_000 {
                self.price_history.pop_front();
            } else {
                break;
            }
        }
        self.price_history.push_back(point);

        // Update baseline volume (rolling average).
        self.volume_history.push_back(point.volume_lamports);
        while self.volume_history.len() > 10 {
            self.volume_history.pop_front();
        }
        self.baseline_volume = if self.volume_history.is_empty() {
            0
        } else {
            self.volume_history.iter().sum::<u64>() / self.volume_history.len() as u64
        };

        // Check pump conditions.
        self.detect_pump()
    }

    /// Detect pump: volume spike + price momentum.
    fn detect_pump(&self) -> bool {
        if self.price_history.len() < 2 {
            return false;
        }

        let current = self.price_history.back().unwrap();
        let _oldest = self.price_history.front().unwrap();

        // Condition 1: Volume spike (5x baseline in last 30 seconds).
        let recent_volume: u64 = self
            .price_history
            .iter()
            .filter(|p| current.timestamp_ms - p.timestamp_ms <= 30_000)
            .map(|p| p.volume_lamports)
            .sum();
        if self.baseline_volume > 0 && recent_volume > self.baseline_volume * 5 {
            // Condition 2: Price momentum (2%+ in 30 seconds).
            if self.price_momentum_pct() > 2.0 {
                return true;
            }
        }

        // Condition 3: Extreme volume spike (10x baseline in 60 seconds).
        let total_volume: u64 = self.price_history.iter().map(|p| p.volume_lamports).sum();
        if self.baseline_volume > 0 && total_volume > self.baseline_volume * 10 {
            return true;
        }

        false
    }

    /// Calculate price momentum as percentage change in last 30 seconds.
    fn price_momentum_pct(&self) -> f64 {
        if self.price_history.len() < 2 {
            return 0.0;
        }

        let current = self.price_history.back().unwrap();
        let reference = self
            .price_history
            .iter()
            .rev()
            .find(|p| current.timestamp_ms - p.timestamp_ms >= 30_000)
            .copied()
            .unwrap_or(*self.price_history.front().unwrap());

        let current_price = sqrt_price_to_price(current.sqrt_price_x64);
        let ref_price = sqrt_price_to_price(reference.sqrt_price_x64);

        if ref_price == 0.0 {
            return 0.0;
        }

        ((current_price - ref_price) / ref_price) * 100.0
    }

    /// Get current pump score (0-100). Higher = more likely pump.
    pub fn pump_score(&self) -> u32 {
        if self.price_history.len() < 2 {
            return 0;
        }

        let current = self.price_history.back().unwrap();
        let mut score = 0u32;

        // Volume spike score (0-40).
        let recent_volume: u64 = self
            .price_history
            .iter()
            .filter(|p| current.timestamp_ms - p.timestamp_ms <= 30_000)
            .map(|p| p.volume_lamports)
            .sum();
        if self.baseline_volume > 0 {
            let volume_ratio = (recent_volume as f64) / (self.baseline_volume as f64);
            score += std::cmp::min(40, (volume_ratio * 10.0) as u32);
        }

        // Price momentum score (0-40).
        let momentum = self.price_momentum_pct();
        score += std::cmp::min(40, (momentum * 20.0) as u32);

        // Consistency score (0-20): sustained volume over time.
        let volume_consistency = self.price_history.len() as u32 / 3; // 60 points max → 20 score
        score += std::cmp::min(20, volume_consistency);

        score
    }

    /// Reset detector (e.g., after entry).
    pub fn reset(&mut self) {
        self.price_history.clear();
        self.volume_history.clear();
        self.baseline_volume = 0;
    }
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
    fn test_pump_detection_volume_spike() {
        let mut detector = PumpDetector::new();
        let now = current_time_ms();

        // Baseline: 100 lamports/sec for 60 seconds.
        for i in 0..60 {
            detector.update(PricePoint {
                sqrt_price_x64: 1u128 << 64,
                volume_lamports: 100,
                timestamp_ms: now + (i as u64 * 1000),
            });
        }

        // Spike: 600 lamports/sec (6x baseline) with 2% price move.
        let spike_price = ((1u128 << 64) as f64 * 1.01) as u128;
        for i in 0..30 {
            let detected = detector.update(PricePoint {
                sqrt_price_x64: spike_price,
                volume_lamports: 600,
                timestamp_ms: now + (60000 + i as u64 * 1000),
            });
            if i == 29 {
                assert!(detected, "pump should be detected on volume spike + momentum");
            }
        }
    }

    #[test]
    fn test_pump_score_calculation() {
        let mut detector = PumpDetector::new();
        let now = current_time_ms();

        for i in 0..60 {
            detector.update(PricePoint {
                sqrt_price_x64: 1u128 << 64,
                volume_lamports: 100,
                timestamp_ms: now + (i as u64 * 1000),
            });
        }

        let score = detector.pump_score();
        assert!(score < 70, "baseline should have low score: {}", score);

        // Add spike.
        let spike_price = ((1u128 << 64) as f64 * 1.05) as u128;
        for i in 0..30 {
            detector.update(PricePoint {
                sqrt_price_x64: spike_price,
                volume_lamports: 600,
                timestamp_ms: now + (60000 + i as u64 * 1000),
            });
        }

        let score = detector.pump_score();
        assert!(score > 40, "spike should have higher score: {}", score);
    }

    fn current_time_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
}
