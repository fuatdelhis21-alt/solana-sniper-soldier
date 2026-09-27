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
    /// When true, `quote()` computes the reverse direction (token_mint_1 ->
    /// token_mint_0, i.e. selling the position back for the entry asset)
    /// instead of the default token_mint_0 -> token_mint_1 buy direction.
    /// Used to build the exit/sell transaction with the same adapter that
    /// resolved the pool for entry.
    reversed: bool,
}

impl RaydiumV4ClmmAdapter {
    pub fn new(pool_id: String) -> Self {
        RaydiumV4ClmmAdapter {
            pool_id,
            program_id: RAYDIUM_CLMM_PROGRAM_ID.to_string(),
            accounts: None,
            pool: None,
            reversed: false,
        }
    }

    /// Mark this adapter to quote/build the exit (sell) direction: the
    /// caller must also pass `SwapAccounts` resolved with the mints swapped
    /// (input = the held token, output = the entry asset).
    pub fn reversed(mut self) -> Self {
        self.reversed = true;
        self
    }

    /// Set the full swap account set. Required before `build_transaction`.
    pub fn with_swap_accounts(mut self, accounts: SwapAccounts) -> Self {
        self.accounts = Some(accounts);
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
        // Forward (buy, mint0->mint1) uses the raw price; reverse (sell,
        // mint1->mint0) uses its inverse. compute_output_amount takes a
        // sqrt_price, so invert at the sqrt level: sqrt(1/p) = (2^64)^2 / sqrt_price.
        let (effective_sqrt_price, input_mint, output_mint) = if self.reversed {
            let inv_sqrt_price = ((1u128 << 64) as f64 * (1u128 << 64) as f64
                / sqrt_price as f64) as u128;
            (inv_sqrt_price, pool.token_mint_1, pool.token_mint_0)
        } else {
            (sqrt_price, pool.token_mint_0, pool.token_mint_1)
        };
        let expected_output =
            Self::compute_output_amount(input_amount, effective_sqrt_price, fee_rate);
        Ok(Quote {
            pool_id: self.pool_id.clone(),
            input_mint: input_mint.to_string(),
            output_mint: output_mint.to_string(),
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

        // Preamble (mirrors devnet_swap_test): a fresh wallet has no token
        // accounts, so a swap-only transaction fails simulation with an
        // account-not-found error on every iteration. Ensure both user ATAs
        // exist (idempotent create), and if the input mint is native SOL,
        // wrap the exact swap amount into the input (wSOL) ATA before the
        // swap instruction runs.
        let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)
            .map_err(|e| format!("invalid token program id: {e}"))?;
        let input_mint = Pubkey::from_str(&intent.quote.input_mint)
            .map_err(|e| format!("invalid input mint '{}': {e}", intent.quote.input_mint))?;
        let output_mint = Pubkey::from_str(&intent.quote.output_mint)
            .map_err(|e| format!("invalid output mint '{}': {e}", intent.quote.output_mint))?;
        let mut ixs = Vec::with_capacity(5);
        ixs.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                signer, signer, &input_mint, &token_program,
            ),
        );
        ixs.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                signer, signer, &output_mint, &token_program,
            ),
        );
        if input_mint == spl_token::native_mint::ID {
            let accounts = self
                .accounts
                .as_ref()
                .ok_or("swap accounts not set — call with_swap_accounts first")?;
            ixs.push(solana_sdk::system_instruction::transfer(
                signer,
                &accounts.input_token_account,
                intent.quote.input_amount,
            ));
            ixs.push(
                spl_token::instruction::sync_native(&token_program, &accounts.input_token_account)
                    .map_err(|e| format!("failed to build sync_native instruction: {e}"))?,
            );
        }
        ixs.push(swap_ix);

        let message = Message::new(&ixs, Some(signer));
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
    fn quote_reversed_inverts_price_and_mints() {
        let pool_id = Pubkey::new_unique();
        let mint0 = Pubkey::new_unique();
        let mint1 = Pubkey::new_unique();
        // price(mint1/mint0) = 4.0 => sqrt_price = 2 * 2^64
        let sqrt_price = 2u128 << 64;
        let pool = ResolvedPool {
            pool_id,
            amm_config: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            token_mint_0: mint0,
            token_mint_1: mint1,
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            observation_key: Pubkey::new_unique(),
            mint_decimals_0: 9,
            mint_decimals_1: 9,
            tick_spacing: 1,
            liquidity: 0,
            sqrt_price_x64: sqrt_price,
            tick_current: 0,
        };
        let forward = RaydiumV4ClmmAdapter::new(pool_id.to_string()).with_resolved_pool(pool.clone());
        let fwd_quote = forward.quote(1_000_000, 0).unwrap();
        assert_eq!(fwd_quote.input_mint, mint0.to_string());
        assert_eq!(fwd_quote.output_mint, mint1.to_string());

        let reverse = RaydiumV4ClmmAdapter::new(pool_id.to_string())
            .with_resolved_pool(pool)
            .reversed();
        let rev_quote = reverse.quote(1_000_000, 0).unwrap();
        assert_eq!(rev_quote.input_mint, mint1.to_string());
        assert_eq!(rev_quote.output_mint, mint0.to_string());
        // Reverse price is ~1/4 of forward price (allow for f64 rounding).
        let ratio = fwd_quote.expected_output as f64 / rev_quote.expected_output as f64;
        assert!((ratio - 16.0).abs() < 0.5, "ratio was {ratio}");
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
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            input_amount: 1_000,
            expected_output: 999,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let tx = adapter
            .build_transaction(&intent, &signer, blockhash)
            .unwrap();
        assert_eq!(tx.message.recent_blockhash, blockhash);
        // Preamble: create-ATA(input) + create-ATA(output) + native wrap
        // (transfer + sync_native) + the swap instruction itself.
        assert_eq!(tx.message.instructions.len(), 5);
        // Unsigned: the fee-payer signature slot is a default (all-zero) placeholder.
        assert_eq!(tx.signatures.len(), 1);
        assert_eq!(
            tx.signatures[0],
            solana_sdk::signature::Signature::default()
        );
    }
}
