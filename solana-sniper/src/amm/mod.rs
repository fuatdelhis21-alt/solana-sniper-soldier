//! # AMM Adapter Trait
//!
//! Protocol-agnostic interface for swap execution.
//! Currently implemented for Raydium V4 CLMM.
//! Designed to be extensible for Orca Whirlpool and others.

use serde::{Deserialize, Serialize};

/// A quote for a swap on a given pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub pool_id: String,
    pub input_mint: String,
    pub output_mint: String,
    pub input_amount: u64,
    pub expected_output: u64,
    pub slippage_bps: u64,
}

/// A trade intent built from a quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeIntent {
    pub quote: Quote,
    pub min_output: u64,
    pub pool_state_hash: String,
}

/// AMM protocol adapter trait.
pub trait AmmAdapter: Send + Sync {
    fn protocol_name(&self) -> &'static str;
    fn quote(
        &self,
        input_amount: u64,
        slippage_bps: u64,
    ) -> Result<Quote, Box<dyn std::error::Error>>;
    fn build_intent(&self, quote: Quote) -> Result<TradeIntent, Box<dyn std::error::Error>>;
    fn build_transaction(
        &self,
        intent: &TradeIntent,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>>;
}

/// Raydium V4 CLMM module.
pub mod raydium_v4;
