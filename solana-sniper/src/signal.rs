//! Entry signal: technical indicators over genuine, timestamped pool-price
//! observations, combined into a transparent confidence score with hard
//! vetoes. Never invents candles: every sample is a real on-chain pool price
//! read by this process, at most one per `min_interval_ms`.
//!
//! An indicator without enough samples is *unavailable*, never bullish: the
//! decision is then `NoTrade("warming_up")`. Safety vetoes (overbought,
//! liquidity draining, excessive volatility) override any score.
//!
//! Indicators (all on the price of the held token in units of the pay token;
//! scale-invariant, so decimals do not matter):
//! - EMA fast / slow (trend), MACD 12/26/9 histogram (momentum),
//! - RSI 14, Wilder smoothing (overbought veto, healthy-range bonus),
//! - volatility: standard deviation of log returns (veto above a cap),
//! - liquidity momentum: change of the pay-side vault over the window.
//!
//! No profitability is claimed for this model; it is a quality filter whose
//! effect must be measured (paper first).

use std::collections::VecDeque;

/// One genuine observation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub ts_ms: u64,
    /// Price of the held token in pay-token units (any constant scale).
    pub price: f64,
    /// Pay-side (SOL) vault liquidity in raw units.
    pub liquidity: u64,
}

/// Bounded series of observations, at least `min_interval_ms` apart.
#[derive(Debug, Clone)]
pub struct Series {
    samples: VecDeque<Observation>,
    cap: usize,
    min_interval_ms: u64,
}

impl Series {
    pub fn new(cap: usize, min_interval_ms: u64) -> Self {
        Self {
            samples: VecDeque::with_capacity(cap),
            cap: cap.max(2),
            min_interval_ms,
        }
    }

    /// Add an observation; ignored when too close to the previous one or
    /// not a finite positive price. Returns whether it was stored.
    pub fn push(&mut self, obs: Observation) -> bool {
        if !(obs.price.is_finite() && obs.price > 0.0) {
            return false;
        }
        if let Some(last) = self.samples.back() {
            if obs.ts_ms < last.ts_ms + self.min_interval_ms {
                return false;
            }
        }
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(obs);
        true
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn prices(&self) -> Vec<f64> {
        self.samples.iter().map(|o| o.price).collect()
    }

    pub fn last_ts_ms(&self) -> Option<u64> {
        self.samples.back().map(|o| o.ts_ms)
    }

    fn liquidity_change_bps(&self, window: usize) -> Option<i64> {
        let n = self.samples.len();
        if n < 2 {
            return None;
        }
        let first = self.samples[n.saturating_sub(window.max(2))].liquidity;
        let last = self.samples[n - 1].liquidity;
        if first == 0 {
            return None;
        }
        Some(((last as i128 - first as i128) * 10_000 / first as i128) as i64)
    }
}

/// Price of the held token in pay-token units from the pool's Q64.64 sqrt
/// price (token1/token0). With the pay token as token0 the held token is
/// token1, worth `1 / price`.
pub fn held_token_price(sqrt_price_x64: u128, input_is_token0: bool) -> f64 {
    let s = sqrt_price_x64 as f64 / (1u128 << 64) as f64;
    let p = s * s;
    if input_is_token0 {
        if p > 0.0 {
            1.0 / p
        } else {
            0.0
        }
    } else {
        p
    }
}

/// Exponential moving average of the last value (seeded with the SMA of the
/// first `period` values). `None` until `period` values exist.
pub fn ema(values: &[f64], period: usize) -> Option<f64> {
    ema_series(values, period).last().copied()
}

fn ema_series(values: &[f64], period: usize) -> Vec<f64> {
    if period == 0 || values.len() < period {
        return Vec::new();
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len() - period + 1);
    let mut e = values[..period].iter().sum::<f64>() / period as f64;
    out.push(e);
    for v in &values[period..] {
        e = v * k + e * (1.0 - k);
        out.push(e);
    }
    out
}

/// RSI with Wilder smoothing. `None` until `period + 1` values exist.
pub fn rsi(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period + 1 {
        return None;
    }
    let diffs: Vec<f64> = values.windows(2).map(|w| w[1] - w[0]).collect();
    let mut gain = diffs[..period].iter().filter(|d| **d > 0.0).sum::<f64>() / period as f64;
    let mut loss = -diffs[..period].iter().filter(|d| **d < 0.0).sum::<f64>() / period as f64;
    for d in &diffs[period..] {
        gain = (gain * (period as f64 - 1.0) + d.max(0.0)) / period as f64;
        loss = (loss * (period as f64 - 1.0) + (-d).max(0.0)) / period as f64;
    }
    if loss == 0.0 {
        return Some(if gain == 0.0 { 50.0 } else { 100.0 });
    }
    Some(100.0 - 100.0 / (1.0 + gain / loss))
}

/// MACD (fast, slow, signal): `(macd, signal, histogram)` of the last value.
/// `None` until `slow + signal - 1` values exist.
pub fn macd(values: &[f64], fast: usize, slow: usize, signal: usize) -> Option<(f64, f64, f64)> {
    if fast >= slow || values.len() < slow + signal - 1 {
        return None;
    }
    let f = ema_series(values, fast);
    let s = ema_series(values, slow);
    // Align: s[i] corresponds to values[slow-1+i], f[j] to values[fast-1+j].
    let offset = slow - fast;
    let line: Vec<f64> = s
        .iter()
        .enumerate()
        .map(|(i, sv)| f[i + offset] - sv)
        .collect();
    let sig = ema(&line, signal)?;
    let m = *line.last()?;
    Some((m, sig, m - sig))
}

/// Standard deviation of log returns over the last `window` samples, in bps.
pub fn volatility_bps(values: &[f64], window: usize) -> Option<f64> {
    if values.len() < window.max(3) {
        return None;
    }
    let tail = &values[values.len() - window.max(3)..];
    let rets: Vec<f64> = tail.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rets.len() as f64;
    Some(var.sqrt() * 10_000.0)
}

/// Configuration of the signal gate.
#[derive(Debug, Clone)]
pub struct SignalConfig {
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub rsi_period: usize,
    /// RSI at or above this vetoes the entry (overbought / pump top).
    pub rsi_max: f64,
    /// Volatility (bps per sample) at or above this vetoes the entry.
    pub max_volatility_bps: f64,
    /// Liquidity drop over the window (bps) at or below this vetoes.
    pub min_liquidity_change_bps: i64,
    /// Minimum confidence (0..=100) to trade.
    pub min_confidence: u32,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            ema_fast: 5,
            ema_slow: 20,
            rsi_period: 14,
            rsi_max: 75.0,
            max_volatility_bps: 800.0,
            min_liquidity_change_bps: -1_000,
            min_confidence: 70,
        }
    }
}

impl SignalConfig {
    /// Samples needed before every indicator is available (MACD 12/26/9).
    pub fn warmup_samples(&self) -> usize {
        self.ema_slow.max(26 + 9 - 1).max(self.rsi_period + 1)
    }
}

/// Outcome of the signal gate.
#[derive(Debug, Clone, PartialEq)]
pub enum SignalDecision {
    Trade {
        confidence: u32,
    },
    NoTrade {
        reason: &'static str,
        confidence: u32,
    },
}

impl SignalDecision {
    pub fn code(&self) -> &'static str {
        match self {
            SignalDecision::Trade { .. } => "trade",
            SignalDecision::NoTrade { reason, .. } => reason,
        }
    }
}

/// Evaluate the gate on the observed series. Vetoes first, then the score:
/// trend (EMA fast > slow) 40, MACD histogram > 0 30, RSI in 45..rsi_max 20,
/// non-negative liquidity momentum 10.
pub fn evaluate(series: &Series, cfg: &SignalConfig) -> SignalDecision {
    if series.len() < cfg.warmup_samples() {
        return SignalDecision::NoTrade {
            reason: "warming_up",
            confidence: 0,
        };
    }
    let prices = series.prices();
    let (Some(fast), Some(slow), Some(r), Some((_, _, hist)), Some(vol)) = (
        ema(&prices, cfg.ema_fast),
        ema(&prices, cfg.ema_slow),
        rsi(&prices, cfg.rsi_period),
        macd(&prices, 12, 26, 9),
        volatility_bps(&prices, cfg.ema_slow),
    ) else {
        return SignalDecision::NoTrade {
            reason: "indicator_unavailable",
            confidence: 0,
        };
    };
    let liq = series.liquidity_change_bps(cfg.ema_slow).unwrap_or(0);
    // Hard vetoes.
    if r >= cfg.rsi_max {
        return SignalDecision::NoTrade {
            reason: "veto_overbought",
            confidence: 0,
        };
    }
    if vol >= cfg.max_volatility_bps {
        return SignalDecision::NoTrade {
            reason: "veto_volatility",
            confidence: 0,
        };
    }
    if liq <= cfg.min_liquidity_change_bps {
        return SignalDecision::NoTrade {
            reason: "veto_liquidity_draining",
            confidence: 0,
        };
    }
    let mut score = 0u32;
    if fast > slow {
        score += 40;
    }
    if hist > 0.0 {
        score += 30;
    }
    if (45.0..cfg.rsi_max).contains(&r) {
        score += 20;
    }
    if liq >= 0 {
        score += 10;
    }
    if score >= cfg.min_confidence {
        SignalDecision::Trade { confidence: score }
    } else {
        SignalDecision::NoTrade {
            reason: "low_confidence",
            confidence: score,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100, then repeating `steps` (realistic: rises with pullbacks).
    fn zigzag(n: usize, steps: &[f64]) -> Vec<f64> {
        let mut p = 100.0;
        (0..n)
            .map(|i| {
                if i > 0 {
                    p += steps[(i - 1) % steps.len()];
                }
                p
            })
            .collect()
    }

    fn series_from(prices: &[f64], liq: impl Fn(usize) -> u64) -> Series {
        let mut s = Series::new(200, 1_000);
        for (i, p) in prices.iter().enumerate() {
            s.push(Observation {
                ts_ms: i as u64 * 1_000,
                price: *p,
                liquidity: liq(i),
            });
        }
        s
    }

    #[test]
    fn ema_rsi_macd_known_values() {
        assert_eq!(ema(&[1.0, 2.0], 3), None);
        assert!((ema(&[1.0, 2.0, 3.0], 3).unwrap() - 2.0).abs() < 1e-12);
        // Seed SMA(1,2,3)=2, then 4*0.5 + 2*0.5 = 3.
        assert!((ema(&[1.0, 2.0, 3.0, 4.0], 3).unwrap() - 3.0).abs() < 1e-12);
        // Monotonic up: RSI 100; flat: 50; monotonic down: 0.
        let up: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(rsi(&up, 14), Some(100.0));
        assert_eq!(rsi(&[5.0; 20], 14), Some(50.0));
        let down: Vec<f64> = (1..=20).rev().map(f64::from).collect();
        assert!(rsi(&down, 14).unwrap() < 1e-9);
        assert_eq!(rsi(&up[..14], 14), None);
        // MACD needs slow + signal - 1 = 34 values; rising series -> positive.
        assert_eq!(macd(&up, 12, 26, 9), None);
        let up40: Vec<f64> = (1..=40).map(|i| 100.0 * 1.01f64.powi(i)).collect();
        let (m, _, _) = macd(&up40, 12, 26, 9).unwrap();
        assert!(m > 0.0);
        assert_eq!(volatility_bps(&[1.0; 10], 5), Some(0.0));
    }

    #[test]
    fn series_rejects_too_frequent_and_invalid_samples_and_is_bounded() {
        let mut s = Series::new(3, 1_000);
        let o = |t, p| Observation {
            ts_ms: t,
            price: p,
            liquidity: 1,
        };
        assert!(s.push(o(0, 1.0)));
        assert!(!s.push(o(500, 1.0)), "too close");
        assert!(!s.push(o(2_000, f64::NAN)));
        assert!(!s.push(o(2_000, 0.0)));
        assert!(s.push(o(1_000, 2.0)));
        assert!(s.push(o(2_000, 3.0)));
        assert!(s.push(o(3_000, 4.0)));
        assert_eq!(s.prices(), vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn held_price_follows_orientation() {
        let one = 1u128 << 64;
        assert!((held_token_price(one * 2, false) - 4.0).abs() < 1e-9);
        assert!((held_token_price(one * 2, true) - 0.25).abs() < 1e-9);
        assert_eq!(held_token_price(0, true), 0.0);
    }

    #[test]
    fn gate_requires_warmup_and_never_defaults_to_trade() {
        let cfg = SignalConfig::default();
        let s = series_from(&[1.0; 10], |_| 100);
        assert_eq!(
            evaluate(&s, &cfg),
            SignalDecision::NoTrade {
                reason: "warming_up",
                confidence: 0
            }
        );
    }

    #[test]
    fn steady_uptrend_with_stable_liquidity_trades() {
        let cfg = SignalConfig::default();
        // Uptrend with pullbacks (+1, +1, -1.2): trend up, RSI ~62.
        let prices = zigzag(60, &[1.0, 1.0, -1.2]);
        let s = series_from(&prices, |i| 1_000 + i as u64);
        match evaluate(&s, &cfg) {
            SignalDecision::Trade { confidence } => assert!(confidence >= 70),
            other => panic!("expected trade, got {other:?}"),
        }
    }

    #[test]
    fn vetoes_override_the_score() {
        let cfg = SignalConfig::default();
        // Pump straight up: RSI 100 -> overbought veto.
        let pump: Vec<f64> = (0..60).map(|i| 100.0 * 1.05f64.powi(i)).collect();
        assert_eq!(
            evaluate(&series_from(&pump, |_| 1_000), &cfg).code(),
            "veto_overbought"
        );
        // Same gentle trend, liquidity halves -> draining veto.
        let prices = zigzag(60, &[1.0, 1.0, -1.2]);
        let s = series_from(&prices, |i| if i < 50 { 1_000 } else { 500 });
        assert_eq!(evaluate(&s, &cfg).code(), "veto_liquidity_draining");
        // Wild swings -> volatility veto.
        let wild: Vec<f64> = (0..60)
            .map(|i| if i % 2 == 0 { 100.0 } else { 115.0 })
            .collect();
        assert_eq!(
            evaluate(&series_from(&wild, |_| 1_000), &cfg).code(),
            "veto_volatility"
        );
        // Downtrend: no veto, but low confidence -> no trade.
        let down = zigzag(60, &[-1.0, -1.0, 1.2]);
        assert_eq!(
            evaluate(&series_from(&down, |_| 1_000), &cfg).code(),
            "low_confidence"
        );
    }
}
