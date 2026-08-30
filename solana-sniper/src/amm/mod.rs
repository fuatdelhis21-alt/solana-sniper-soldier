//! # AMM Adapter Trait
//!
//! Protocol-agnostic interface for swap execution.
//! Currently implemented for Raydium V4 CLMM.
//! Designed to be extensible for Orca Whirlpool and others.

use serde::{Deserialize, Serialize};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;

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
    /// Build an unsigned transaction for the given intent, signed by `signer`
    /// and using `blockhash`. The returned transaction is ready to be signed
    /// by the HSM (fail-closed: no local keyfile in live mode).
    fn build_transaction(
        &self,
        intent: &TradeIntent,
        signer: &Pubkey,
        blockhash: Hash,
    ) -> Result<Transaction, Box<dyn std::error::Error>>;
}

/// Raydium V4 CLMM module.
pub mod raydium_v4;
