/// # Alpha Discovery Scoring Engine
///
/// Token scoring and alpha discovery logic for HFT decision-making.
/// Computes a composite alpha score from liquidity, holder concentration,
/// and developer activity metrics.

/// Token score structure used for alpha discovery.
pub struct TokenScore {
    pub liquidity_score: f64,
    pub holder_concentration: f64,
    pub dev_activity: f64,
}

impl TokenScore {
    /// Create a new TokenScore with given metrics.
    pub fn new(liquidity_score: f64, holder_concentration: f64, dev_activity: f64) -> Self {
        Self {
            liquidity_score,
            holder_concentration,
            dev_activity,
        }
    }

    /// Calculate composite alpha score (0.0 - 1.0 scale).
    ///
    /// Formula:
    /// - 50% weight on liquidity score
    /// - 40% weight on inverse holder concentration (low concentration = higher score)
    /// - 10% weight on developer activity
    pub fn calculate_alpha_score(&self) -> f64 {
        (self.liquidity_score * 0.5)
            + ((1.0 - self.holder_concentration) * 0.4)
            + (self.dev_activity * 0.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpha_score_high_liquidity() {
        let score = TokenScore::new(1.0, 0.1, 0.8);
        let alpha = score.calculate_alpha_score();
        // Expected: 1.0*0.5 + (1.0-0.1)*0.4 + 0.8*0.1 = 0.5 + 0.36 + 0.08 = 0.94
        assert!((alpha - 0.94).abs() < 1e-9);
    }

    #[test]
    fn test_alpha_score_low_liquidity() {
        let score = TokenScore::new(0.2, 0.9, 0.1);
        let alpha = score.calculate_alpha_score();
        // Expected: 0.2*0.5 + (1.0-0.9)*0.4 + 0.1*0.1 = 0.1 + 0.04 + 0.01 = 0.15
        assert!((alpha - 0.15).abs() < 1e-9);
    }
}
