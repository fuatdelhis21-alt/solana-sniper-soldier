use crate::risk::RiskFirewall;
use hft_core::scoring::TokenScore;

/// Evaluate alpha score and run pre-trade risk check.
/// Parameters are primitive to avoid tight coupling with TradeIntent types:
/// - amount_sol: trade size in SOL
/// - liquidity_score: normalized liquidity signal [0..1]
/// - holder_concentration: 0..1 (1.0 = extremely concentrated)
/// - dev_activity: normalized dev activity [0..1]
pub fn evaluate_and_check(
    amount_sol: f64,
    liquidity_score: f64,
    holder_concentration: f64,
    dev_activity: f64,
    risk: &mut RiskFirewall,
) -> Result<f64, String> {
    // 1) Risk check: may return Err if circuit-breaker tripped or size too large
    risk.pre_trade_check(amount_sol)?;

    // 2) Alpha scoring using TokenScore
    let ts = TokenScore {
        liquidity_score,
        holder_concentration,
        dev_activity,
    };
    let score = ts.calculate_alpha_score();

    Ok(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskFirewall;

    #[test]
    fn test_evaluate_and_check_ok() {
        let mut rf = RiskFirewall {
            max_trade_sol: 1.0,
            daily_loss_limit_sol: 10.0,
            current_loss_sol: 0.0,
        };
        let res = evaluate_and_check(0.5, 0.8, 0.2, 0.4, &mut rf).expect("should pass risk");
        assert!(res > 0.0);
    }

    #[test]
    fn test_evaluate_and_check_blocked_by_size() {
        let mut rf = RiskFirewall {
            max_trade_sol: 0.1,
            daily_loss_limit_sol: 10.0,
            current_loss_sol: 0.0,
        };
        let err = evaluate_and_check(0.5, 0.8, 0.2, 0.4, &mut rf).unwrap_err();
        assert!(err.to_lowercase().contains("max"));
    }

    #[test]
    fn test_evaluate_and_check_blocked_by_daily_loss() {
        let mut rf = RiskFirewall {
            max_trade_sol: 10.0,
            daily_loss_limit_sol: 1.0,
            current_loss_sol: 2.0,
        };
        let err = evaluate_and_check(0.1, 0.8, 0.2, 0.4, &mut rf).unwrap_err();
        assert!(
            err.to_lowercase().contains("circuit breaker") || err.to_lowercase().contains("günlük")
        );
    }
}
