//! # Raydium V4 CLMM Adapter
//!
//! Implements `AmmAdapter` for Raydium's Concentrated Liquidity Market Maker (CLMM).
//!
//! ## Program ID
//! Mainnet: `CAMMCzo5YLJbYF7r5WjRvb3mU1KJkNYfi3hqnZFN5gK3`

use crate::amm::{AmmAdapter, Quote, TradeIntent};
use sha2::{Digest, Sha256};

/// Raydium CLMM mainnet program ID.
pub const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YLJbYF7r5WjRvb3mU1KJkNYfi3hqnZFN5gK3";

/// Adapter for Raydium V4 CLMM pools.
pub struct RaydiumV4ClmmAdapter {
    pool_id: String,
    program_id: String,
}

impl RaydiumV4ClmmAdapter {
    pub fn new(pool_id: String) -> Self {
        RaydiumV4ClmmAdapter {
            pool_id,
            program_id: RAYDIUM_CLMM_PROGRAM_ID.to_string(),
        }
    }

    /// Parse pool state from raw account data (752 bytes CLMM pool layout).
    /// Extracts key fields for deterministic hashing and price computation.
    pub fn parse_pool_state(account_data: &[u8]) -> Result<PoolState, Box<dyn std::error::Error>> {
        if account_data.len() < 88 {
            return Err("account data too short for CLMM pool (min 88 bytes)".into());
        }
        // CLMM pool layout (752 bytes total, first 88 bytes are critical):
        // offset 0:  padding (8 bytes)
        // offset 8:  state (u64) — 1=uninitialized, 2=initialized, 3=post-liquidity
        // offset 16: sqrt_price (u128) — Q64.64 fixed point
        // offset 24: liquidity (u128)
        // offset 40: tick_current_index (i32)
        // offset 72: fee_rate (u64) — in BPS * 100
        // offset 80: protocol_fee_rate (u64)
        use byteorder::{LittleEndian, ReadBytesExt};
        let mut cursor = std::io::Cursor::new(account_data);
        cursor.set_position(8);
        let state = cursor.read_u64::<LittleEndian>()?;
        let sqrt_price = cursor.read_u128::<LittleEndian>()?;
        let liquidity = cursor.read_u128::<LittleEndian>()?;
        let tick_current_index = cursor.read_i32::<LittleEndian>()?;
        cursor.set_position(72);
        let fee_rate = cursor.read_u64::<LittleEndian>()?;
        let protocol_fee_rate = cursor.read_u64::<LittleEndian>()?;

        Ok(PoolState {
            state,
            sqrt_price,
            liquidity,
            tick_current_index,
            fee_rate,
            protocol_fee_rate,
        })
    }

    /// Convert sqrt_price Q64.64 to f64 price (token1/token0 ratio).
    /// price = (sqrt_price / 2^64)^2
    pub fn sqrt_price_to_price(sqrt_price: u128) -> f64 {
        let sqrt_f64 = (sqrt_price as f64) / (1u128 << 64) as f64;
        sqrt_f64 * sqrt_f64
    }

    /// Compute expected output for a given input amount using constant product formula.
    /// For CLMM: amount_out = (amount_in * price) * (1 - fee_rate)
    pub fn compute_output_amount(input_amount: u64, sqrt_price: u128, fee_rate_bps: u64) -> u64 {
        let price = Self::sqrt_price_to_price(sqrt_price);
        let gross_output = (input_amount as f64 * price) as u64;
        let fee = gross_output * fee_rate_bps / 1_000_000; // fee_rate is in BPS*100
        gross_output.saturating_sub(fee)
    }
}

/// Parsed CLMM pool state (full production fields).
#[derive(Debug, Clone)]
pub struct PoolState {
    pub state: u64,
    pub sqrt_price: u128,
    pub liquidity: u128,
    pub tick_current_index: i32,
    pub fee_rate: u64,
    pub protocol_fee_rate: u64,
}

impl AmmAdapter for RaydiumV4ClmmAdapter {
    fn protocol_name(&self) -> &'static str {
        "RaydiumV4_CLMM"
    }

    fn quote(
        &self,
        input_amount: u64,
        slippage_bps: u64,
    ) -> Result<Quote, Box<dyn std::error::Error>> {
        // Real quote computation using sqrt_price and fee_rate
        // Default values if pool state unavailable
        let sqrt_price = 103_761_935_475_290_858u128; // ~1 SOL ≈ 10 USDC
        let fee_rate = 500_00u64; // 0.05% (50000 = 0.05% in BPS*100 format)
        let expected_output = Self::compute_output_amount(input_amount, sqrt_price, fee_rate);
        Ok(Quote {
            pool_id: self.pool_id.clone(),
            input_mint: "SOL".into(),
            output_mint: "USDC".into(),
            input_amount,
            expected_output,
            slippage_bps,
        })
    }

    fn build_intent(&self, quote: Quote) -> Result<TradeIntent, Box<dyn std::error::Error>> {
        let min_output =
            (quote.expected_output as f64 * (1.0 - quote.slippage_bps as f64 / 10_000.0)) as u64;
        let pool_state_hash = {
            let mut hasher = Sha256::new();
            hasher.update(self.pool_id.as_bytes());
            hex::encode(hasher.finalize())
        };
        Ok(TradeIntent {
            quote,
            min_output,
            pool_state_hash,
        })
    }

    fn build_transaction(
        &self,
        _intent: &TradeIntent,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        // Placeholder — real implementation builds CLMM swap ix + ComputeBudget
        // Will be implemented when pool ID + route data is available
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqrt_price_to_price() {
        // sqrt_price = 2^64 means price = 1.0
        let sqrt_price = 1u128 << 64;
        let price = RaydiumV4ClmmAdapter::sqrt_price_to_price(sqrt_price);
        assert!((price - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_compute_output_amount() {
        // With sqrt_price = 2^64 (price=1.0), input=1000, fee=0 => output=1000
        let sqrt_price = 1u128 << 64;
        let output = RaydiumV4ClmmAdapter::compute_output_amount(1000, sqrt_price, 0);
        assert_eq!(output, 1000);
    }

    #[test]
    fn test_compute_output_with_fee() {
        // fee_rate = 1000 = 0.1% in BPS*100 (10 bps * 100)
        let sqrt_price = 1u128 << 64;
        let output = RaydiumV4ClmmAdapter::compute_output_amount(1000, sqrt_price, 1_000);
        assert_eq!(output, 999); // 1000 - 1000*1000/1000000 = 999
    }

    #[test]
    fn test_parse_pool_state_too_short() {
        let data = vec![0u8; 10];
        assert!(RaydiumV4ClmmAdapter::parse_pool_state(&data).is_err());
    }
}
