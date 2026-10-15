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
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            // 1000 SOL liquidity floor (conservative for devnet testing).
            min_liquidity_lamports: 1_000_000_000_000,
            // 0.01 SOL max position per trade — deliberately BELOW the
            // production risk cap (0.05 SOL, risk::RiskConfig::
            // production_defaults). Invariant:
            //   strategy_position_size <= risk.max_trade_size_lamports
            // is enforced at startup (main.rs) and by the unit tests here,
            // so the two constants can never silently diverge again.
            max_trade_size_lamports: 10_000_000,
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
            // NOTE: no `min_holders` — `getTokenLargestAccounts` returns at
            // most 20 accounts per the SPL JSON-RPC standard, so an absolute
            // holder count is structurally unmeasurable. Rug-risk is gated
            // by the holder-CONCENTRATION check (30% single / 70% top-20,
            // onchain_risk::MAX_*_HOLDER_PCT) applied in the data layer for
            // both paper and live when --live-risk-data is on.
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
    /// Exit because the position reached its maximum holding time
    /// (live only; set by the caller, never by the price checks).
    MaxHoldTime,
    /// Exit because the SOL-side pool liquidity fell far below its level at
    /// entry (liquidity being pulled — the classic new-pool rug). Set by the
    /// caller via [`liquidity_dropped`], never by the price checks.
    LiquidityDrop,
}

/// SOL-side liquidity drop (bps of the entry level) that forces an exit.
pub const LIQUIDITY_DROP_EXIT_BPS: u64 = 5_000;

/// True when `current` liquidity is at least [`LIQUIDITY_DROP_EXIT_BPS`]
/// below `at_entry`. `at_entry == 0` (unknown, e.g. an older position
/// record) never triggers.
pub fn liquidity_dropped(at_entry: u64, current: u64) -> bool {
    if at_entry == 0 {
        return false;
    }
    let floor = at_entry as u128 * (10_000 - LIQUIDITY_DROP_EXIT_BPS) as u128 / 10_000;
    (current as u128) <= floor
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
        // NOTE: no absolute holder-count check here — see StrategyConfig
        // default docs. Holder risk is gated by the concentration check in
        // the data layer (onchain_risk::holder_concentration_verdict) with
        // an explicit RejectReason, never a silent no_entry_signal.

        Some(EntrySignal {
            position_size_lamports: self.config.max_trade_size_lamports,
            slippage_bps: self.config.max_slippage_bps,
            entry_sqrt_price,
        })
    }

    /// Decide whether to exit an open position given the current price.
    ///
    /// `current_sqrt_price` and `entry_sqrt_price` are Q64.64 fixed-point.
    /// The price ratio `(current/entry)^2` comes from [`price_ratio_scaled`]
    /// (integer, overflow-checked; the paper P&L uses the very same value)
    /// and is compared exactly against the bps thresholds. A zero entry or a
    /// ratio too large to represent (corrupt data) is `Hold`: no decision is
    /// taken on data that cannot be evaluated.
    pub fn should_exit(&self, entry_sqrt_price: u128, current_sqrt_price: u128) -> ExitDecision {
        match price_ratio_scaled(entry_sqrt_price, current_sqrt_price) {
            Some(ratio) => self.exit_on_ratio(ratio),
            None => ExitDecision::Hold,
        }
    }

    /// Value-based exit decision for a REAL position: `cost` is what the
    /// entry actually spent and `current_value` what selling the held tokens
    /// would return right now (both in input-token units, e.g. lamports).
    /// Unlike `should_exit` this is direction-independent — it does not
    /// matter whether the bought token is the pool's token0 or token1 — and
    /// it includes the pool fee on the way out. Same integer threshold rule.
    pub fn should_exit_value(&self, cost: u64, current_value: u64) -> ExitDecision {
        match mul_div_floor(current_value as u128, RATIO_SCALE, cost as u128) {
            Some(ratio) => self.exit_on_ratio(ratio),
            None => ExitDecision::Hold, // cost == 0
        }
    }

    /// Thresholds on a ratio scaled by [`RATIO_SCALE`]: take-profit when the
    /// gain is >= `take_profit_bps`, stop-loss when the loss is >=
    /// `stop_loss_bps`.
    fn exit_on_ratio(&self, ratio: u128) -> ExitDecision {
        let tp = bps_to_scaled(self.config.take_profit_bps);
        let sl = bps_to_scaled(self.config.stop_loss_bps);
        if ratio >= RATIO_SCALE {
            if ratio - RATIO_SCALE >= tp {
                return ExitDecision::TakeProfit;
            }
        } else if RATIO_SCALE - ratio >= sl {
            return ExitDecision::StopLoss;
        }
        ExitDecision::Hold
    }
}

/// Fixed-point scale of price ratios: 1.0 == 10^12 (1 bp == 10^8).
pub const RATIO_SCALE: u128 = 1_000_000_000_000;

fn bps_to_scaled(bps: u64) -> u128 {
    bps as u128 * (RATIO_SCALE / 10_000)
}

/// `floor(a * b / d)` with a full 256-bit intermediate product. `None` when
/// `d == 0` or the quotient does not fit in a u128. Never panics.
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    mul_div(a, b, d).map(|(q, _)| q)
}

/// `ceil(a * b / d)`, same contract as [`mul_div_floor`].
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let (q, r) = mul_div(a, b, d)?;
    if r == 0 {
        Some(q)
    } else {
        q.checked_add(1)
    }
}

/// (quotient, remainder) of `a * b / d` via 256-bit schoolbook arithmetic.
fn mul_div(a: u128, b: u128, d: u128) -> Option<(u128, u128)> {
    if d == 0 {
        return None;
    }
    const M: u128 = u64::MAX as u128;
    let (a0, a1, b0, b1) = (a & M, a >> 64, b & M, b >> 64);
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let mid = (p00 >> 64) + (p01 & M) + (p10 & M);
    let lo = (p00 & M) | (mid << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    if hi >= d {
        return None; // quotient >= 2^128
    }
    if hi == 0 {
        return Some((lo / d, lo % d));
    }
    // Long division of (hi, lo) by d, one bit at a time; hi < d keeps the
    // quotient within 128 bits.
    let (mut rem, mut q) = (hi, 0u128);
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        q <<= 1;
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            q |= 1;
        }
    }
    Some((q, rem))
}

/// Price ratio `(current_sqrt / entry_sqrt)^2` scaled by [`RATIO_SCALE`],
/// in integer arithmetic. `None` for a zero entry or a ratio that does not
/// fit (more than ~3.4e14 x — only corrupt data gets there). The rounding
/// error is below 10^-11 relative, far under one basis point.
pub fn price_ratio_scaled(entry_sqrt_price: u128, current_sqrt_price: u128) -> Option<u128> {
    let sqrt_ratio = mul_div_floor(current_sqrt_price, RATIO_SCALE, entry_sqrt_price)?;
    mul_div_floor(sqrt_ratio, sqrt_ratio, RATIO_SCALE)
}

/// P&L (lamports) of `position_lamports` when the price moved by `ratio`
/// (scaled by [`RATIO_SCALE`]). Gains round down, losses round up (away
/// from zero): never optimistic. Saturates at `i64::MAX`; a loss can never
/// exceed the position.
pub fn pnl_from_ratio(ratio: u128, position_lamports: u64) -> i64 {
    let pos = position_lamports as u128;
    if ratio >= RATIO_SCALE {
        let gain = mul_div_floor(pos, ratio - RATIO_SCALE, RATIO_SCALE).unwrap_or(u128::MAX);
        i64::try_from(gain).unwrap_or(i64::MAX)
    } else {
        // (SCALE - ratio) <= SCALE, so the loss is <= pos (a u64); above
        // i64::MAX it saturates instead of wrapping.
        let loss = mul_div_ceil(pos, RATIO_SCALE - ratio, RATIO_SCALE)
            .unwrap_or(pos)
            .min(pos);
        -i64::try_from(loss).unwrap_or(i64::MAX)
    }
}

/// Fail-closed cross-module invariant, checked at startup (main.rs) and in
/// tests: the strategy must never size a position above the risk manager's
/// hard per-trade cap, otherwise every entry would be rejected at the
/// `pre_trade_check` gate (MaxSpendExceeded) — or worse, silently bypass it.
/// Breaking this invariant is a configuration error: refuse to start.
pub fn validate_position_size_invariant(
    strategy_position_size_lamports: u64,
    risk_max_trade_size_lamports: u64,
) -> Result<(), String> {
    if strategy_position_size_lamports > risk_max_trade_size_lamports {
        return Err(format!(
            "config error: strategy position size ({strategy_position_size_lamports} lamports) \
             exceeds risk max_trade_size_lamports ({risk_max_trade_size_lamports} lamports); \
             refusing to start (fail-closed)"
        ));
    }
    Ok(())
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
        assert_eq!(sig.position_size_lamports, 10_000_000);
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
    fn evaluate_no_longer_gates_on_absolute_holder_count() {
        // Regression (operator-approved design): `min_holders` (50) was
        // removed because getTokenLargestAccounts caps at 20 accounts, so an
        // absolute holder count can never be measured. Holder risk is gated
        // by the concentration check in onchain_risk::holder_concentration_
        // verdict (30% single / 70% top-20) in the data layer.
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let mut c = valid_candidate();
        c.holders = 10; // would have been rejected by the old min_holders=50
        assert!(s.evaluate(&c, 1u128 << 64).is_some());
    }

    #[test]
    fn should_exit_value_thresholds() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        assert_eq!(
            s.should_exit_value(10_000_000, 10_000_000),
            ExitDecision::Hold
        );
        assert_eq!(
            s.should_exit_value(10_000_000, 11_000_000),
            ExitDecision::TakeProfit
        );
        assert_eq!(
            s.should_exit_value(10_000_000, 9_500_000),
            ExitDecision::StopLoss
        );
        assert_eq!(
            s.should_exit_value(10_000_000, 9_600_000),
            ExitDecision::Hold
        );
        assert_eq!(s.should_exit_value(0, 1), ExitDecision::Hold);
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
        // Just above +10% (an exactly-10% fixture built with f64 sqrt lands
        // a hair below the boundary; the boundary itself is tested exactly
        // in exit_thresholds_are_exact_at_the_boundaries).
        let sqrt_110 = (1.1001f64).sqrt();
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

    // ── Trade-size vs risk-cap invariant (fail-closed) ──

    #[test]
    fn strategy_default_position_size_stays_below_production_risk_limit() {
        use crate::risk::RiskConfig;
        let risk_cfg =
            RiskConfig::production_defaults(std::env::temp_dir().join("size_inv_test")).unwrap();
        let strat_cfg = StrategyConfig::default();
        assert!(
            strat_cfg.max_trade_size_lamports <= risk_cfg.max_trade_size_lamports,
            "strategy {strat_cfg:?} must never exceed risk cap {}",
            risk_cfg.max_trade_size_lamports
        );
        // Document the exact safe values so a future bump is deliberate.
        assert_eq!(strat_cfg.max_trade_size_lamports, 10_000_000, "0.01 SOL");
        assert_eq!(risk_cfg.max_trade_size_lamports, 50_000_000, "0.05 SOL");
    }

    #[test]
    fn oversized_strategy_configuration_is_fail_closed_rejected() {
        let err = validate_position_size_invariant(100_000_000, 50_000_000).unwrap_err();
        assert!(err.contains("config error"), "got: {err}");
        assert!(err.contains("refusing to start"), "got: {err}");
        // Equal sizes are allowed; only exceeding the risk cap is an error.
        assert!(validate_position_size_invariant(50_000_000, 50_000_000).is_ok());
        assert!(validate_position_size_invariant(10_000_000, 50_000_000).is_ok());
    }

    #[test]
    fn liquidity_drop_threshold() {
        assert!(
            !liquidity_dropped(0, 0),
            "unknown entry liquidity never triggers"
        );
        assert!(!liquidity_dropped(100, 51));
        assert!(liquidity_dropped(100, 50));
        assert!(liquidity_dropped(100, 0));
        assert!(!liquidity_dropped(100, 500), "rising liquidity is fine");
        assert!(liquidity_dropped(u64::MAX, 1));
    }

    // ── Integer price-ratio math ──

    #[test]
    fn mul_div_matches_exact_arithmetic_and_never_panics() {
        for (a, b, d) in [
            (0u128, 5u128, 3u128),
            (7, 9, 4),
            (u64::MAX as u128, u64::MAX as u128, 3),
            (1 << 100, 1 << 27, 1 << 30),
        ] {
            let exact = a * b;
            assert_eq!(mul_div_floor(a, b, d), Some(exact / d));
            assert_eq!(
                mul_div_ceil(a, b, d),
                Some(exact / d + u128::from(exact % d != 0))
            );
        }
        // 256-bit intermediates.
        assert_eq!(
            mul_div_floor(u128::MAX, u128::MAX, u128::MAX),
            Some(u128::MAX)
        );
        assert_eq!(
            mul_div_floor(u128::MAX, 1 << 64, 1 << 65),
            Some(u128::MAX >> 1)
        );
        assert_eq!(mul_div_floor(1 << 127, 6, 3), None, "2^128 does not fit");
        assert_eq!(mul_div_floor(u128::MAX, 2, 1), None);
        assert_eq!(mul_div_floor(1, 1, 0), None);
        assert_eq!(mul_div_ceil(u128::MAX, 1, 1), Some(u128::MAX));
        assert_eq!(mul_div_ceil(u128::MAX, 3, 2), None);
    }

    #[test]
    fn price_ratio_handles_zero_huge_and_overflowing_prices() {
        let one = 1u128 << 64;
        assert_eq!(price_ratio_scaled(one, one), Some(RATIO_SCALE));
        assert_eq!(price_ratio_scaled(one, 2 * one), Some(4 * RATIO_SCALE));
        assert_eq!(price_ratio_scaled(2 * one, one), Some(RATIO_SCALE / 4));
        assert_eq!(price_ratio_scaled(0, one), None, "zero entry");
        assert_eq!(price_ratio_scaled(one, 0), Some(0), "price went to zero");
        // Near-maximal Q64.64 values on both sides stay exact.
        assert_eq!(price_ratio_scaled(u128::MAX, u128::MAX), Some(RATIO_SCALE));
        assert_eq!(
            price_ratio_scaled(u128::MAX, u128::MAX / 2),
            Some(RATIO_SCALE / 4 - 1), // floor
        );
        // Absurd ratios do not fit: None, never a panic or a wrap.
        assert_eq!(price_ratio_scaled(1, u128::MAX), None);
        assert_eq!(price_ratio_scaled(1, 1 << 80), None);
    }

    #[test]
    fn pnl_from_ratio_is_signed_bounded_and_conservative() {
        assert_eq!(pnl_from_ratio(RATIO_SCALE, 1_000_000_000), 0);
        assert_eq!(
            pnl_from_ratio(4 * RATIO_SCALE, 1_000_000_000),
            3_000_000_000
        );
        assert_eq!(pnl_from_ratio(RATIO_SCALE / 4, 1_000_000_000), -750_000_000);
        assert_eq!(
            pnl_from_ratio(0, 1_000_000_000),
            -1_000_000_000,
            "total loss"
        );
        // Losses round away from zero, gains toward zero.
        assert_eq!(pnl_from_ratio(RATIO_SCALE - 1, 1), -1);
        assert_eq!(pnl_from_ratio(RATIO_SCALE + 1, 1), 0);
        // Saturation instead of overflow.
        assert_eq!(pnl_from_ratio(u128::MAX, u64::MAX), i64::MAX);
        assert_eq!(pnl_from_ratio(0, u64::MAX), -i64::MAX, "no sign wrap");
        assert_eq!(pnl_from_ratio(RATIO_SCALE / 2, u64::MAX), -i64::MAX);
    }

    #[test]
    fn exit_thresholds_are_exact_at_the_boundaries() {
        let s = SimpleSnipeStrategy::new(StrategyConfig::default());
        let one = RATIO_SCALE;
        let bp = RATIO_SCALE / 10_000;
        // take-profit 1000 bps, stop-loss 500 bps (defaults).
        assert_eq!(s.exit_on_ratio(one + 1_000 * bp), ExitDecision::TakeProfit);
        assert_eq!(s.exit_on_ratio(one + 1_000 * bp - 1), ExitDecision::Hold);
        assert_eq!(s.exit_on_ratio(one - 500 * bp), ExitDecision::StopLoss);
        assert_eq!(s.exit_on_ratio(one - 500 * bp + 1), ExitDecision::Hold);
        // should_exit and the paper P&L read the same ratio.
        let (e, c) = (3u128 << 70, 7u128 << 69);
        let r = price_ratio_scaled(e, c).unwrap();
        assert_eq!(s.should_exit(e, c), s.exit_on_ratio(r));
        assert_eq!(
            s.should_exit(1, u128::MAX),
            ExitDecision::Hold,
            "unrepresentable"
        );
        assert_eq!(s.should_exit_value(0, 5), ExitDecision::Hold);
        assert_eq!(s.should_exit_value(u64::MAX, u64::MAX), ExitDecision::Hold);
    }
}
