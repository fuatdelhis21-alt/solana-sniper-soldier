//! # Raydium V4 CLMM Adapter
//!
//! Implements `AmmAdapter` for Raydium's Concentrated Liquidity Market Maker (CLMM).
//!
//! ## Program ID
//! Mainnet: `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` (single source of
//! truth: [`crate::amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID`]).

use crate::amm::account_resolver::ResolvedPool;
use crate::amm::{AmmAdapter, Quote, TradeIntent};
use sha2::{Digest, Sha256};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use std::str::FromStr;

/// Raydium CLMM mainnet program ID — re-exported from the account resolver so
/// the swap instruction and the PDA derivations can never disagree again
/// (the old constant here matched no mainnet pool, so every swap would have
/// been sent to a non-existent program).
pub use crate::amm::account_resolver::RAYDIUM_CLMM_PROGRAM_ID;
/// SPL Token program ID.
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// Associated Token Account program ID.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
/// Wrapped SOL (native) mint.
pub const NATIVE_MINT: &str = "So11111111111111111111111111111111111111112";

/// Transaction-level options for a real, sendable swap transaction.
#[derive(Debug, Clone)]
pub struct SwapTxOptions {
    /// Mint the user pays with (the swap input side).
    pub input_mint: Pubkey,
    /// Mint the user receives (the swap output side).
    pub output_mint: Pubkey,
    /// Compute unit limit (0 = leave the runtime default, no instruction).
    pub compute_unit_limit: u32,
    /// Priority fee in micro-lamports per CU (0 = no priority fee).
    pub priority_fee_micro_lamports: u64,
    /// Close the (non-native) input token account after the swap. Set on an
    /// exit that sells the whole balance, so the ATA rent comes back too.
    pub close_input_token_account: bool,
    /// Jito tip `(tip account, lamports)` appended as the last instruction
    /// when submitting through a Block Engine (bundles without a tip are
    /// dropped). `None` for plain RPC submission.
    pub jito_tip: Option<(Pubkey, u64)>,
}

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
    /// First initialized tick array in the swap direction.
    pub tick_array: Pubkey,
    /// Further initialized tick arrays in the swap direction, passed as
    /// remaining accounts so a swap may cross array boundaries.
    pub extra_tick_arrays: Vec<Pubkey>,
}

/// Adapter for Raydium V4 CLMM pools.
pub struct RaydiumV4ClmmAdapter {
    pool_id: String,
    program_id: String,
    accounts: Option<SwapAccounts>,
    pool: Option<ResolvedPool>,
}

impl RaydiumV4ClmmAdapter {
    pub fn new(pool_id: String) -> Self {
        RaydiumV4ClmmAdapter {
            pool_id,
            program_id: RAYDIUM_CLMM_PROGRAM_ID.to_string(),
            accounts: None,
            pool: None,
        }
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

        let mut metas = vec![
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
        metas.extend(
            accounts
                .extra_tick_arrays
                .iter()
                .map(|pk| AccountMeta::new(*pk, false)),
        );

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

    /// True when the swap input is the pool's token0 (selling token0 for
    /// token1). Derived from the resolved swap accounts' input vault; without
    /// accounts the legacy token0 -> token1 direction is assumed.
    pub fn input_is_token0(&self) -> bool {
        match (&self.accounts, &self.pool) {
            (Some(a), Some(p)) => a.input_vault != p.token_vault_1,
            _ => true,
        }
    }

    /// Expected token0 output when selling `input_amount` of token1:
    /// amount_out = (amount_in / price) * (1 - fee_rate).
    pub fn compute_output_amount_reverse(
        input_amount: u64,
        sqrt_price: u128,
        fee_rate_bps: u64,
    ) -> u64 {
        let price = Self::sqrt_price_to_price(sqrt_price);
        if price <= 0.0 {
            return 0;
        }
        let gross_output = (input_amount as f64 / price) as u64;
        let fee = gross_output * fee_rate_bps / 1_000_000;
        gross_output.saturating_sub(fee)
    }

    /// Build a complete, sendable swap transaction: compute budget (limit +
    /// priority fee), idempotent creation of the user's input/output ATAs,
    /// SOL wrapping when paying with native SOL, the CLMM swap itself, and
    /// unwrapping (closing) any wSOL account at the end so proceeds land as
    /// plain lamports. Without the ATA/wSOL setup a buy of a fresh token
    /// fails on-chain because the output token account does not exist.
    pub fn build_swap_transaction(
        &self,
        intent: &TradeIntent,
        signer: &Pubkey,
        blockhash: Hash,
        opts: &SwapTxOptions,
    ) -> Result<Transaction, Box<dyn std::error::Error>> {
        let accounts = self
            .accounts
            .as_ref()
            .ok_or("swap accounts not set — call with_swap_accounts first")?;
        let native_mint = Pubkey::from_str(NATIVE_MINT)?;
        let mut ixs: Vec<Instruction> = Vec::with_capacity(8);

        if opts.compute_unit_limit > 0 {
            ixs.push(
                solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
                    opts.compute_unit_limit,
                ),
            );
        }
        if opts.priority_fee_micro_lamports > 0 {
            ixs.push(
                solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(
                    opts.priority_fee_micro_lamports,
                ),
            );
        }

        ixs.push(create_ata_idempotent_ix(
            signer,
            &accounts.input_token_account,
            &opts.input_mint,
        )?);
        ixs.push(create_ata_idempotent_ix(
            signer,
            &accounts.output_token_account,
            &opts.output_mint,
        )?);

        if opts.input_mint == native_mint {
            ixs.push(solana_sdk::system_instruction::transfer(
                signer,
                &accounts.input_token_account,
                intent.quote.input_amount,
            ));
            ixs.push(sync_native_ix(&accounts.input_token_account)?);
        }

        ixs.push(self.build_swap_instruction(intent, signer)?);

        if opts.input_mint == native_mint || opts.close_input_token_account {
            ixs.push(close_account_ix(&accounts.input_token_account, signer)?);
        }
        if opts.output_mint == native_mint {
            ixs.push(close_account_ix(&accounts.output_token_account, signer)?);
        }
        if let Some((tip_account, lamports)) = opts.jito_tip {
            ixs.push(solana_sdk::system_instruction::transfer(
                signer,
                &tip_account,
                lamports,
            ));
        }

        let message = Message::new(&ixs, Some(signer));
        let mut tx = Transaction::new_unsigned(message);
        tx.message.recent_blockhash = blockhash;
        Ok(tx)
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

/// ATA program `CreateIdempotent` (instruction index 1): creates the
/// associated token account if missing, no-op if it already exists.
fn create_ata_idempotent_ix(
    payer: &Pubkey,
    ata: &Pubkey,
    mint: &Pubkey,
) -> Result<Instruction, Box<dyn std::error::Error>> {
    let owner = payer;
    Ok(Instruction {
        program_id: Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM_ID)?,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(Pubkey::from_str(TOKEN_PROGRAM_ID)?, false),
        ],
        data: vec![1],
    })
}

/// SPL Token `SyncNative` (instruction 17): sync a wSOL account's token
/// amount with its lamport balance after a SOL transfer into it.
fn sync_native_ix(account: &Pubkey) -> Result<Instruction, Box<dyn std::error::Error>> {
    Ok(Instruction {
        program_id: Pubkey::from_str(TOKEN_PROGRAM_ID)?,
        accounts: vec![AccountMeta::new(*account, false)],
        data: vec![17],
    })
}

/// SPL Token `CloseAccount` (instruction 9): closes a (wSOL) token account,
/// returning its lamports to the owner — this is the "unwrap" step.
fn close_account_ix(
    account: &Pubkey,
    owner: &Pubkey,
) -> Result<Instruction, Box<dyn std::error::Error>> {
    Ok(Instruction {
        program_id: Pubkey::from_str(TOKEN_PROGRAM_ID)?,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*owner, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9],
    })
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
        // The pool's real fee tier (parts per million) read from its
        // AmmConfig by resolve_swap_accounts; 500 (0.05%) only when no
        // config was resolved (unit tests / offline).
        let fee_rate = pool.trade_fee_rate.map(u64::from).unwrap_or(500);
        // Direction: the pool price is token1/token0. Selling token0 yields
        // `amount * price` token1; selling token1 yields `amount / price`
        // token0. The direction is taken from the resolved swap accounts
        // (input vault), so the exit (reverse) swap is quoted correctly too.
        let input_is_token0 = self.input_is_token0();
        let expected_output = if input_is_token0 {
            Self::compute_output_amount(input_amount, sqrt_price, fee_rate)
        } else {
            Self::compute_output_amount_reverse(input_amount, sqrt_price, fee_rate)
        };
        let (input_mint, output_mint) = if input_is_token0 {
            (pool.token_mint_0, pool.token_mint_1)
        } else {
            (pool.token_mint_1, pool.token_mint_0)
        };
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
            extra_tick_arrays: vec![],
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
    fn extra_tick_arrays_are_appended_as_writable_remaining_accounts() {
        let mut adapter = sample_adapter();
        let extra = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        adapter.accounts.as_mut().unwrap().extra_tick_arrays = extra.clone();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: "SOL".into(),
            output_mint: "X".into(),
            input_amount: 1_000,
            expected_output: 999,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let ix = adapter
            .build_swap_instruction(&intent, &Pubkey::new_unique())
            .unwrap();
        assert_eq!(ix.accounts.len(), 12);
        assert_eq!(ix.accounts[10].pubkey, extra[0]);
        assert_eq!(ix.accounts[11].pubkey, extra[1]);
        assert!(ix.accounts[10].is_writable && !ix.accounts[10].is_signer);
    }

    #[test]
    fn quote_uses_pool_fee_tier_when_resolved() {
        let mut data = vec![0u8; 273];
        data[253..269].copy_from_slice(&(1u128 << 64).to_le_bytes());
        let mut pool =
            RaydiumV4ClmmAdapter::parse_pool_state(&Pubkey::new_unique(), &data).unwrap();
        pool.trade_fee_rate = Some(10_000); // 1% tier
        let q = sample_adapter()
            .with_resolved_pool(pool)
            .quote(1_000_000, 100)
            .unwrap();
        assert_eq!(q.expected_output, 990_000);
    }

    #[test]
    fn swap_program_id_is_canonical_mainnet_clmm() {
        assert_eq!(
            RAYDIUM_CLMM_PROGRAM_ID,
            "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"
        );
        assert_eq!(
            RaydiumV4ClmmAdapter::new("p".into()).program_id,
            RAYDIUM_CLMM_PROGRAM_ID
        );
    }

    #[test]
    fn reverse_output_inverts_price() {
        // sqrt = 2 * 2^64 => price = 4 token1 per token0.
        let sqrt = 2u128 << 64;
        assert_eq!(
            RaydiumV4ClmmAdapter::compute_output_amount(1_000, sqrt, 0),
            4_000
        );
        assert_eq!(
            RaydiumV4ClmmAdapter::compute_output_amount_reverse(4_000, sqrt, 0),
            1_000
        );
    }

    #[test]
    fn quote_direction_follows_input_vault() {
        let mut data = vec![0u8; 273];
        data[253..269].copy_from_slice(&(2u128 << 64).to_le_bytes());
        let pool_id = Pubkey::new_unique();
        let pool = RaydiumV4ClmmAdapter::parse_pool_state(&pool_id, &data).unwrap();
        // Input vault == token_vault_1 => selling token1 for token0.
        let mut accounts = sample_adapter().accounts.unwrap();
        accounts.input_vault = pool.token_vault_1;
        accounts.output_vault = Pubkey::new_unique();
        let adapter = RaydiumV4ClmmAdapter::new("p".into())
            .with_swap_accounts(accounts)
            .with_resolved_pool(pool);
        assert!(!adapter.input_is_token0());
        let q = adapter.quote(4_000_000, 100).unwrap();
        // price 4 => ~1_000_000 token0 minus 0.05% fee.
        assert!(q.expected_output <= 1_000_000 && q.expected_output > 999_000);
    }

    #[test]
    fn build_swap_transaction_wraps_sol_and_creates_atas() {
        let adapter = sample_adapter();
        let accounts = adapter.accounts.clone().unwrap();
        let signer = Pubkey::new_unique();
        let native = Pubkey::from_str(NATIVE_MINT).unwrap();
        let token = Pubkey::new_unique();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: native.to_string(),
            output_mint: token.to_string(),
            input_amount: 10_000_000,
            expected_output: 1,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let opts = SwapTxOptions {
            input_mint: native,
            output_mint: token,
            compute_unit_limit: 300_000,
            priority_fee_micro_lamports: 10_000,
            close_input_token_account: false,
            jito_tip: None,
        };
        let tx = adapter
            .build_swap_transaction(&intent, &signer, Hash::new_unique(), &opts)
            .unwrap();
        let keys = &tx.message.account_keys;
        let programs: Vec<Pubkey> = tx
            .message
            .instructions
            .iter()
            .map(|ix| keys[ix.program_id_index as usize])
            .collect();
        // cu limit, cu price, ata(in), ata(out), transfer, sync, swap, close(wSOL in)
        assert_eq!(programs.len(), 8);
        assert_eq!(
            programs[6],
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap()
        );
        assert_eq!(programs[4], solana_sdk::system_program::id());
        // Fee payer is the signer; wSOL ATA receives the transfer.
        assert_eq!(keys[0], signer);
        assert!(keys.contains(&accounts.input_token_account));
        assert!(keys.contains(&accounts.output_token_account));
    }

    #[test]
    fn build_swap_transaction_unwraps_sol_on_exit() {
        let adapter = sample_adapter();
        let signer = Pubkey::new_unique();
        let native = Pubkey::from_str(NATIVE_MINT).unwrap();
        let token = Pubkey::new_unique();
        let quote = Quote {
            pool_id: "pool_test".into(),
            input_mint: token.to_string(),
            output_mint: native.to_string(),
            input_amount: 5_000,
            expected_output: 1,
            slippage_bps: 100,
        };
        let intent = adapter.build_intent(quote).unwrap();
        let opts = SwapTxOptions {
            input_mint: token,
            output_mint: native,
            compute_unit_limit: 0,
            priority_fee_micro_lamports: 0,
            close_input_token_account: false,
            jito_tip: None,
        };
        let tx = adapter
            .build_swap_transaction(&intent, &signer, Hash::new_unique(), &opts)
            .unwrap();
        // ata(in), ata(out), swap, close(wSOL out) — no wrap for a token input.
        assert_eq!(tx.message.instructions.len(), 4);
        let last = tx.message.instructions.last().unwrap();
        assert_eq!(last.data, vec![9]);

        // With a Jito tip the transfer to the tip account is the last ix.
        let tip_account = Pubkey::new_unique();
        let tipped = SwapTxOptions {
            jito_tip: Some((tip_account, 10_000)),
            ..opts.clone()
        };
        let tx = adapter
            .build_swap_transaction(&intent, &signer, Hash::new_unique(), &tipped)
            .unwrap();
        let last = tx.message.instructions.last().unwrap();
        let keys = &tx.message.account_keys;
        assert_eq!(
            keys[last.program_id_index as usize],
            solana_sdk::system_program::id()
        );
        assert_eq!(keys[last.accounts[1] as usize], tip_account);

        // Full exit: the emptied token ATA is closed too (rent refunded).
        let full = SwapTxOptions {
            close_input_token_account: true,
            ..opts
        };
        let tx = adapter
            .build_swap_transaction(&intent, &signer, Hash::new_unique(), &full)
            .unwrap();
        assert_eq!(tx.message.instructions.len(), 5);
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
