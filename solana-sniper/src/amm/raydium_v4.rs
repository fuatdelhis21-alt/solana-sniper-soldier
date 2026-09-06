//! # Raydium V4 CLMM Adapter
//!
//! Implements `AmmAdapter` for Raydium's Concentrated Liquidity Market Maker (CLMM).
//!
//! ## Program ID
//! Mainnet: `CAMMCzo5YLJbYF7r5WjRvb3mU1KJkNYfi3hqnZFN5gK3`

use crate::amm::account_resolver::ResolvedPool;
use crate::amm::{AmmAdapter, Quote, TradeIntent};
use sha2::{Digest, Sha256};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use std::str::FromStr;

/// Raydium CLMM mainnet program ID.
pub const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YLJbYF7r5WjRvb3mU1KJkNYfi3hqnZFN5gK3";
/// SPL Token program ID.
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Anchor discriminator for `global:swap` = sha256("global:swap")[0..8].
pub const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// The full account set required by the Raydium CLMM `swap` instruction.
#[derive(Debug, Clone)]
pub struct SwapAccounts {
    /// AMM config (factory state) account.
    pub amm_config: Pubkey,
    /// Pool state account.
    pub pool_state: Pubkey,
    /// User's input token account (ATA).
    pub input_token_account: Pubkey,
    /// User's output token account (ATA).
    pub output_token_account: Pubkey,
    /// Pool's input token vault.
    pub input_vault: Pubkey,
    /// Pool's output token vault.
    pub output_vault: Pubkey,
    /// Observation (oracle) state account.
    pub observation_state: Pubkey,
    /// Tick array account for the current tick.
    pub tick_array: Pubkey,
}

/// Adapter for Raydium V4 CLMM pools.
pub struct RaydiumV4ClmmAdapter {
    pool_id: String,
    program_id: String,
    accounts: Option<SwapAccounts>,
    pool: Option<ResolvedPool>,
    /// Which of the pool's two mints is the swap *input*. `None` keeps the
    /// legacy default direction token0 → token1. `Some(mint)` makes
    /// `quote` compute the direction-aware output (token1 → token0 flips the
    /// price) and report input/output mints that match the actual swap.
    input_mint: Option<Pubkey>,
}

impl RaydiumV4ClmmAdapter {
    pub fn new(pool_id: String) -> Self {
        RaydiumV4ClmmAdapter {
            pool_id,
            program_id: RAYDIUM_CLMM_PROGRAM_ID.to_string(),
            accounts: None,
            pool: None,
            input_mint: None,
        }
    }

    /// Set the full swap account set. Required before `build_transaction`.
    pub fn with_swap_accounts(mut self, accounts: SwapAccounts) -> Self {
        self.accounts = Some(accounts);
        self
    }

    /// Declare which of the pool's mints is the input side of the swap, so
    /// `quote` prices the correct direction (selling token1 for token0 must
    /// divide by the token0/token1 price, not multiply). Fail-closed at
    /// quote time if the mint is not one of the pool's two mints.
    pub fn with_input_mint(mut self, mint: Pubkey) -> Self {
        self.input_mint = Some(mint);
        self
    }

    /// Override the CLMM program id used to build the swap instruction.
    /// Required for devnet (mainnet program id does not exist on devnet).
    pub fn with_program_id(mut self, program_id: String) -> Self {
        self.program_id = program_id;
        self
    }

    /// Attach a resolved pool state so `quote` uses the real on-chain price.
    pub fn with_resolved_pool(mut self, pool: ResolvedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// The resolved pool state, if attached.
    pub fn resolved_pool(&self) -> Option<&ResolvedPool> {
        self.pool.as_ref()
    }

    /// Build the Raydium CLMM `swap` instruction.
    ///
    /// Data layout (LE):
    /// - 8-byte discriminator (sha256("global:swap")[0..8])
    /// - amount: u64
    /// - other_amount_threshold: u64 (min output for slippage protection)
    /// - sqrt_price_limit_x64: u128 (Q64.64 price limit; 0 = no limit)
    /// - is_base_input: bool
    pub fn build_swap_instruction(
        &self,
        intent: &TradeIntent,
        signer: &Pubkey,
    ) -> Result<Instruction, Box<dyn std::error::Error>> {
        let accounts = self
            .accounts
            .as_ref()
            .ok_or("swap accounts not set — call with_swap_accounts first")?;

        let amount = intent.quote.input_amount;
        let other_amount_threshold = intent.min_output;
        let sqrt_price_limit_x64: u128 = 0; // no price limit (slippage via threshold)
        let is_base_input: bool = true;

        let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1);
        data.extend_from_slice(&SWAP_DISCRIMINATOR);
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&other_amount_threshold.to_le_bytes());
        data.extend_from_slice(&sqrt_price_limit_x64.to_le_bytes());
        data.push(is_base_input as u8);

        let program_id =
            Pubkey::from_str(&self.program_id).map_err(|e| format!("invalid program id: {e}"))?;
        let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)
            .map_err(|e| format!("invalid token program id: {e}"))?;

        let metas = vec![
            AccountMeta::new(*signer, true), // payer (signer)
            AccountMeta::new_readonly(accounts.amm_config, false),
            AccountMeta::new(accounts.pool_state, false),
            AccountMeta::new(accounts.input_token_account, false),
            AccountMeta::new(accounts.output_token_account, false),
            AccountMeta::new(accounts.input_vault, false),
            AccountMeta::new(accounts.output_vault, false),
            AccountMeta::new(accounts.observation_state, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new(accounts.tick_array, false),
        ];

        Ok(Instruction {
            program_id,
            accounts: metas,
            data,
        })
    }

    /// Parse pool state from raw account data using the real CLMM zero_copy
    /// layout. Delegates to [`crate::amm::account_resolver::parse_pool_state`].
    pub fn parse_pool_state(pool_id: &Pubkey, account_data: &[u8]) -> Result<ResolvedPool, String> {
        crate::amm::account_resolver::parse_pool_state(pool_id, account_data)
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

impl AmmAdapter for RaydiumV4ClmmAdapter {
    fn protocol_name(&self) -> &'static str {
        "RaydiumV4_CLMM"
    }

    fn quote(
        &self,
        input_amount: u64,
        slippage_bps: u64,
    ) -> Result<Quote, Box<dyn std::error::Error>> {
        // Real quote computation using the resolved pool's on-chain sqrt_price.
        // If no pool is attached, fail closed rather than quote a fabricated price.
        let pool = self
            .pool
            .as_ref()
            .ok_or("no resolved pool attached — call with_resolved_pool before quote")?;
        let sqrt_price = pool.sqrt_price_x64;
        // 0.05% = 5 bps, in BPS*100 format = 500. (fee = gross * 500 / 1_000_000)
        let fee_rate = 500u64;

        // Direction-aware pricing: the tx always sells `input_mint` (when
        // declared). token0 → token1 multiplies by price; token1 → token0
        // divides by it. The legacy default (input_mint = None) keeps the
        // token0 → token1 convention for backward compatibility.
        let (input_mint_str, output_mint_str, expected_output) = match self.input_mint {
            Some(m) if m == pool.token_mint_0 => (
                pool.token_mint_0.to_string(),
                pool.token_mint_1.to_string(),
                Self::compute_output_amount(input_amount, sqrt_price, fee_rate),
            ),
            Some(m) if m == pool.token_mint_1 => {
                let price = Self::sqrt_price_to_price(sqrt_price);
                // Fail-closed: a zero/non-finite price cannot price the
                // reverse direction — never fabricate an output.
                if !(price > 0.0) || !price.is_finite() {
                    return Err(format!(
                        "cannot price token1 → token0: pool price is not positive/finite (sqrt={sqrt_price})"
                    )
                    .into());
                }
                let gross_output = ((input_amount as f64) / price) as u64;
                let fee = gross_output.saturating_mul(fee_rate) / 1_000_000;
                (
                    pool.token_mint_1.to_string(),
                    pool.token_mint_0.to_string(),
                    gross_output.saturating_sub(fee),
                )
            }
            Some(_) => {
                return Err(
                    "input mint is not one of the pool's two mints — refusing to quote (fail-closed)"
                        .into(),
                );
            }
            None => (
                pool.token_mint_0.to_string(),
                pool.token_mint_1.to_string(),
                Self::compute_output_amount(input_amount, sqrt_price, fee_rate),
            ),
        };

        Ok(Quote {
            pool_id: self.pool_id.clone(),
            input_mint: input_mint_str,
            output_mint: output_mint_str,
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
        intent: &TradeIntent,
        signer: &Pubkey,
        blockhash: Hash,
    ) -> Result<Transaction, Box<dyn std::error::Error>> {
        let swap_ix = self.build_swap_instruction(intent, signer)?;
        let message = Message::new(&[swap_ix], Some(signer));
        let mut tx = Transaction::new_unsigned(message);
        tx.message.recent_blockhash = blockhash;
        Ok(tx)
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
        let pool_id = Pubkey::new_unique();
        assert!(RaydiumV4ClmmAdapter::parse_pool_state(&pool_id, &data).is_err());
    }

    #[test]
    fn quote_fails_closed_without_resolved_pool() {
        let adapter = sample_adapter();
        assert!(adapter.quote(1_000_000, 100).is_err());
    }

    #[test]
    fn quote_uses_resolved_pool_price() {
        // Build a synthetic pool with sqrt_price = 2^64 (price = 1.0).
        let mut data = vec![0u8; 273];
        data[253..269].copy_from_slice(&(1u128 << 64).to_le_bytes());
        let pool_id = Pubkey::new_unique();
        let pool = RaydiumV4ClmmAdapter::parse_pool_state(&pool_id, &data).unwrap();
        let adapter = sample_adapter().with_resolved_pool(pool);
        let quote = adapter.quote(1_000_000, 100).unwrap();
        // price=1.0, fee 0.05% => output slightly below input.
        assert!(quote.expected_output < 1_000_000);
        assert!(quote.expected_output > 990_000);
    }

    #[test]
    fn quote_token1_to_token0_divides_by_price() {
        let pool = synthetic_pool_with(1u128 << 65); // price = 4.0
        let adapter = sample_adapter()
            .with_resolved_pool(pool.clone())
            .with_input_mint(pool.token_mint_1);
        // Selling 1_000_000 of token1 at price 4.0 (token1 per token0) =>
        // ~250_000 token0 before the 0.05% fee.
        let quote = adapter.quote(1_000_000, 100).unwrap();
        assert_eq!(quote.input_mint, pool.token_mint_1.to_string());
        assert_eq!(quote.output_mint, pool.token_mint_0.to_string());
        assert!(quote.expected_output < 250_000);
        assert!(quote.expected_output > 248_000);
    }

    #[test]
    fn quote_token0_to_token1_multiplies_by_price() {
        let pool = synthetic_pool_with(1u128 << 65); // price = 4.0
        let adapter = sample_adapter()
            .with_resolved_pool(pool.clone())
            .with_input_mint(pool.token_mint_0);
        // Selling 1_000_000 of token0 at price 4.0 => ~4_000_000 token1.
        let quote = adapter.quote(1_000_000, 100).unwrap();
        assert_eq!(quote.input_mint, pool.token_mint_0.to_string());
        assert_eq!(quote.output_mint, pool.token_mint_1.to_string());
        assert!(quote.expected_output > 3_980_000);
        assert!(quote.expected_output < 4_000_000);
    }

    #[test]
    fn quote_rejects_input_mint_not_in_pool() {
        let pool = synthetic_pool_with(1u128 << 64);
        let adapter = sample_adapter()
            .with_resolved_pool(pool)
            .with_input_mint(Pubkey::new_unique());
        assert!(adapter.quote(1_000_000, 100).is_err());
    }

    #[test]
    fn quote_token1_to_token0_rejects_zero_price() {
        // sqrt_price = 0 => price = 0; reverse pricing must fail closed.
        let pool = synthetic_pool_with(0);
        let adapter = sample_adapter()
            .with_resolved_pool(pool.clone())
            .with_input_mint(pool.token_mint_1);
        assert!(adapter.quote(1_000_000, 100).is_err());
    }

    /// Synthetic pool state with distinct token mints (mint_0 @ byte 73,
    /// mint_1 @ byte 105 per the real CLMM zero_copy layout) so direction
    /// arms can be exercised independently.
    fn synthetic_pool_with(sqrt_price_x64: u128) -> ResolvedPool {
        let mut data = vec![0u8; 273];
        data[73..105].copy_from_slice(&[7u8; 32]); // token_mint_0
        data[105..137].copy_from_slice(&[9u8; 32]); // token_mint_1
        data[253..269].copy_from_slice(&sqrt_price_x64.to_le_bytes());
        let pool_id = Pubkey::new_unique();
        RaydiumV4ClmmAdapter::parse_pool_state(&pool_id, &data).unwrap()
    }

    fn sample_adapter() -> RaydiumV4ClmmAdapter {
        let accounts = SwapAccounts {
            amm_config: Pubkey::new_unique(),
            pool_state: Pubkey::new_unique(),
            input_token_account: Pubkey::new_unique(),
            output_token_account: Pubkey::new_unique(),
            input_vault: Pubkey::new_unique(),
            output_vault: Pubkey::new_unique(),
            observation_state: Pubkey::new_unique(),
            tick_array: Pubkey::new_unique(),
        };
        RaydiumV4ClmmAdapter::new("pool_test".to_string()).with_swap_accounts(accounts)
    }

    #[test]
    fn swap_discriminator_matches_anchor() {
        // sha256("global:swap")[0..8] must equal SWAP_DISCRIMINATOR.
        use sha2::Digest;
        let mut h = Sha256::new();
        h.update(b"global:swap");
        let digest = h.finalize();
        assert_eq!(SWAP_DISCRIMINATOR, digest[..8]);
    }

    #[test]
    fn build_swap_instruction_encodes_data_correctly() {
        let adapter = sample_adapter();
        let signer = Pubkey::new_unique();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: "SOL".into(),
            output_mint: "USDC".into(),
            input_amount: 1_000_000,
            expected_output: 999_000,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let ix = adapter.build_swap_instruction(&intent, &signer).unwrap();

        // Data: 8-byte discriminator + amount(u64) + threshold(u64) + sqrt_limit(u128) + is_base_input(bool)
        assert_eq!(ix.data.len(), 8 + 8 + 8 + 16 + 1);
        assert_eq!(&ix.data[0..8], &SWAP_DISCRIMINATOR);
        // amount = 1_000_000 (LE)
        assert_eq!(&ix.data[8..16], &1_000_000u64.to_le_bytes());
        // other_amount_threshold = min_output (LE)
        assert_eq!(&ix.data[16..24], &intent.min_output.to_le_bytes());
        // sqrt_price_limit = 0 (LE u128)
        assert_eq!(&ix.data[24..40], &0u128.to_le_bytes());
        // is_base_input = true
        assert_eq!(ix.data[40], 1u8);

        // Program id is the Raydium CLMM program.
        assert_eq!(ix.program_id.to_string(), RAYDIUM_CLMM_PROGRAM_ID);
    }

    #[test]
    fn build_swap_instruction_requires_accounts() {
        let adapter = RaydiumV4ClmmAdapter::new("pool_test".to_string());
        let signer = Pubkey::new_unique();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: "SOL".into(),
            output_mint: "USDC".into(),
            input_amount: 1_000,
            expected_output: 999,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        assert!(adapter.build_swap_instruction(&intent, &signer).is_err());
    }

    #[test]
    fn build_swap_instruction_account_metas_order() {
        let adapter = sample_adapter();
        let signer = Pubkey::new_unique();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: "SOL".into(),
            output_mint: "USDC".into(),
            input_amount: 1_000,
            expected_output: 999,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let ix = adapter.build_swap_instruction(&intent, &signer).unwrap();

        // 10 accounts in the documented order.
        assert_eq!(ix.accounts.len(), 10);
        // payer is the signer and is a signer.
        assert_eq!(ix.accounts[0].pubkey, signer);
        assert!(ix.accounts[0].is_signer);
        // token_program is readonly.
        let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID).unwrap();
        assert_eq!(ix.accounts[8].pubkey, token_program);
        assert!(!ix.accounts[8].is_signer);
    }

    #[test]
    fn build_transaction_returns_unsigned_with_blockhash() {
        let adapter = sample_adapter();
        let signer = Pubkey::new_unique();
        let blockhash = Hash::new_unique();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: "SOL".into(),
            output_mint: "USDC".into(),
            input_amount: 1_000,
            expected_output: 999,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let tx = adapter
            .build_transaction(&intent, &signer, blockhash)
            .unwrap();
        assert_eq!(tx.message.recent_blockhash, blockhash);
        // Unsigned: the fee-payer signature slot is a default (all-zero) placeholder.
        assert_eq!(tx.signatures.len(), 1);
        assert_eq!(
            tx.signatures[0],
            solana_sdk::signature::Signature::default()
        );
    }
}
