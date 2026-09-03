//! # Raydium CLMM Account Resolution
//!
//! Deterministic on-chain account resolution for Raydium CLMM swaps.
//!
//! The full `SwapAccounts` set required by the CLMM `swap` instruction is
//! resolved from the pool state account plus the user's wallet:
//!
//! - **Pool vaults, mints, observation key, amm_config** — read from the pool
//!   state account (Anchor `PoolState`, zero_copy layout).
//! - **User ATAs** — derived deterministically via the SPL Associated Token
//!   Account program (`find_program_address([owner, token_program, mint])`).
//! - **Tick array** — derived via the CLMM tick-array PDA.
//!
//! ## PoolState layout (account-relative offsets, 8-byte anchor discriminator)
//! | field            | offset |
//! |------------------|--------|
//! | amm_config       | 9      |
//! | owner            | 41     |
//! | token_mint_0     | 73     |
//! | token_mint_1     | 105    |
//! | token_vault_0    | 137    |
//! | token_vault_1    | 169    |
//! | observation_key  | 201    |
//! | mint_decimals_0  | 233    |
//! | mint_decimals_1  | 234    |
//! | tick_spacing     | 235    |
//! | liquidity        | 237    |
//! | sqrt_price_x64   | 253    |
//! | tick_current     | 269    |
//!
//! ## Safety
//! - Resolution is deterministic (same pool + wallet → same accounts).
//! - Fail-closed: any parse/derivation error propagates; no partial accounts.

use byteorder::{LittleEndian, ReadBytesExt};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::io::{Cursor, Read};
use std::str::FromStr;

use crate::amm::raydium_v4::SwapAccounts;

/// Raydium CLMM mainnet program ID.
pub const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YLJbYF7r5WjRvb3mU1KJkNYfi3hqnZFN5gK3";
/// Raydium CLMM devnet program ID.
pub const RAYDIUM_CLMM_PROGRAM_ID_DEVNET: &str = "DRayAUgENGQBKVaX8owNhgzkEDyoHTGVEGHVJT1E9pfH";
/// SPL Token program ID.
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// SPL Associated Token Account program ID.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Seed for the tick-array PDA.
pub const TICK_ARRAY_SEED: &[u8] = b"tick_array";
/// Number of ticks per tick array.
pub const TICK_ARRAY_SIZE: i32 = 60;

/// Parsed Raydium CLMM pool state (zero_copy layout, 8-byte discriminator prefix).
#[derive(Debug, Clone)]
pub struct ResolvedPool {
    pub pool_id: Pubkey,
    pub amm_config: Pubkey,
    pub owner: Pubkey,
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    pub observation_key: Pubkey,
    pub mint_decimals_0: u8,
    pub mint_decimals_1: u8,
    pub tick_spacing: u16,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
}

impl ResolvedPool {
    /// Current price as token_1/token_0 (Q64.64 sqrt price squared).
    pub fn price(&self) -> f64 {
        let sqrt = (self.sqrt_price_x64 as f64) / (1u128 << 64) as f64;
        sqrt * sqrt
    }

    /// The tick-array start index for the current tick.
    pub fn tick_array_start_index(&self) -> i32 {
        let ticks_in_array = TICK_ARRAY_SIZE * i32::from(self.tick_spacing);
        // Floor division (not truncation) — required for negative ticks, since
        // Rust's `/` truncates toward zero and Raydium's on-chain program
        // expects the mathematical floor of tick_current / ticks_in_array.
        self.tick_current.div_euclid(ticks_in_array) * ticks_in_array
    }

    /// Derive the tick-array PDA for the current tick.
    pub fn tick_array_pda(&self, program_id: &Pubkey) -> Pubkey {
        let start = self.tick_array_start_index();
        let (pda, _) = Pubkey::find_program_address(
            &[TICK_ARRAY_SEED, self.pool_id.as_ref(), &start.to_be_bytes()],
            program_id,
        );
        pda
    }
}

/// Parse a Raydium CLMM pool state account (raw bytes) into `ResolvedPool`.
pub fn parse_pool_state(pool_id: &Pubkey, data: &[u8]) -> Result<ResolvedPool, String> {
    if data.len() < 273 {
        return Err(format!(
            "pool account data too short for CLMM PoolState: {} bytes (need >= 273)",
            data.len()
        ));
    }
    let mut c = Cursor::new(data);
    c.set_position(9);
    let amm_config = read_pubkey(&mut c)?;
    let owner = read_pubkey(&mut c)?;
    let token_mint_0 = read_pubkey(&mut c)?;
    let token_mint_1 = read_pubkey(&mut c)?;
    let token_vault_0 = read_pubkey(&mut c)?;
    let token_vault_1 = read_pubkey(&mut c)?;
    let observation_key = read_pubkey(&mut c)?;
    c.set_position(233);
    let mint_decimals_0 = c.read_u8().map_err(|e| e.to_string())?;
    let mint_decimals_1 = c.read_u8().map_err(|e| e.to_string())?;
    let tick_spacing = c.read_u16::<LittleEndian>().map_err(|e| e.to_string())?;
    let liquidity = c.read_u128::<LittleEndian>().map_err(|e| e.to_string())?;
    let sqrt_price_x64 = c.read_u128::<LittleEndian>().map_err(|e| e.to_string())?;
    let tick_current = c.read_i32::<LittleEndian>().map_err(|e| e.to_string())?;

    Ok(ResolvedPool {
        pool_id: *pool_id,
        amm_config,
        owner,
        token_mint_0,
        token_mint_1,
        token_vault_0,
        token_vault_1,
        observation_key,
        mint_decimals_0,
        mint_decimals_1,
        tick_spacing,
        liquidity,
        sqrt_price_x64,
        tick_current,
    })
}

fn read_pubkey(c: &mut Cursor<&[u8]>) -> Result<Pubkey, String> {
    let mut buf = [0u8; 32];
    c.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(Pubkey::new_from_array(buf))
}

/// Deterministically resolve the user's associated token account (ATA) for a mint.
///
/// `ATA = find_program_address([owner, token_program, mint], associated_token_program)`.
pub fn resolve_user_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID).expect("valid token program id");
    let ata_program = Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM_ID).expect("valid ata program id");
    let (ata, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );
    ata
}

/// Fetch and parse a pool state account from the RPC.
pub fn fetch_pool_state(rpc: &RpcClient, pool_id: &Pubkey) -> Result<ResolvedPool, String> {
    let account = rpc
        .get_account(pool_id)
        .map_err(|e| format!("failed to fetch pool state {pool_id}: {e}"))?;
    parse_pool_state(pool_id, &account.data)
}

/// Resolve the full swap account set for a CLMM swap.
///
/// `input_mint` / `output_mint` select which side of the pool is the input.
/// Returns the `SwapAccounts` plus the resolved pool (for quoting).
pub fn resolve_swap_accounts(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    user: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<(SwapAccounts, ResolvedPool), String> {
    let pool = fetch_pool_state(rpc, pool_id)?;

    // Determine which vault is the input/output based on the requested mints.
    let (input_vault, output_vault) = if input_mint == &pool.token_mint_0 {
        (pool.token_vault_0, pool.token_vault_1)
    } else if input_mint == &pool.token_mint_1 {
        (pool.token_vault_1, pool.token_vault_0)
    } else {
        return Err(format!(
            "input mint {input_mint} is not a token in pool {pool_id} (mint0={}, mint1={})",
            pool.token_mint_0, pool.token_mint_1
        ));
    };

    let input_token_account = resolve_user_ata(user, input_mint);
    let output_token_account = resolve_user_ata(user, output_mint);
    let tick_array = pool.tick_array_pda(program_id);

    let accounts = SwapAccounts {
        amm_config: pool.amm_config,
        pool_state: *pool_id,
        input_token_account,
        output_token_account,
        input_vault,
        output_vault,
        observation_state: pool.observation_key,
        tick_array,
    };

    Ok((accounts, pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic 273-byte pool state buffer with known field values.
    fn synthetic_pool_data(
        amm_config: &Pubkey,
        mint0: &Pubkey,
        mint1: &Pubkey,
        vault0: &Pubkey,
        vault1: &Pubkey,
        obs: &Pubkey,
        sqrt_price: u128,
        liquidity: u128,
        tick: i32,
        tick_spacing: u16,
    ) -> Vec<u8> {
        let mut data = vec![0u8; 273];
        // discriminator (8 bytes) left as zeros.
        data[9..41].copy_from_slice(amm_config.as_ref());
        data[73..105].copy_from_slice(mint0.as_ref());
        data[105..137].copy_from_slice(mint1.as_ref());
        data[137..169].copy_from_slice(vault0.as_ref());
        data[169..201].copy_from_slice(vault1.as_ref());
        data[201..233].copy_from_slice(obs.as_ref());
        data[233] = 9; // mint_decimals_0
        data[234] = 6; // mint_decimals_1
        data[235..237].copy_from_slice(&tick_spacing.to_le_bytes());
        data[237..253].copy_from_slice(&liquidity.to_le_bytes());
        data[253..269].copy_from_slice(&sqrt_price.to_le_bytes());
        data[269..273].copy_from_slice(&tick.to_le_bytes());
        data
    }

    #[test]
    fn parse_pool_state_roundtrip() {
        let amm_config = Pubkey::new_unique();
        let mint0 = Pubkey::new_unique();
        let mint1 = Pubkey::new_unique();
        let vault0 = Pubkey::new_unique();
        let vault1 = Pubkey::new_unique();
        let obs = Pubkey::new_unique();
        let pool_id = Pubkey::new_unique();
        let sqrt_price = 1u128 << 64;
        let liquidity = 1_000_000u128;
        let tick = 100;
        let tick_spacing = 10;

        let data = synthetic_pool_data(
            &amm_config,
            &mint0,
            &mint1,
            &vault0,
            &vault1,
            &obs,
            sqrt_price,
            liquidity,
            tick,
            tick_spacing,
        );
        let pool = parse_pool_state(&pool_id, &data).unwrap();

        assert_eq!(pool.pool_id, pool_id);
        assert_eq!(pool.amm_config, amm_config);
        assert_eq!(pool.token_mint_0, mint0);
        assert_eq!(pool.token_mint_1, mint1);
        assert_eq!(pool.token_vault_0, vault0);
        assert_eq!(pool.token_vault_1, vault1);
        assert_eq!(pool.observation_key, obs);
        assert_eq!(pool.mint_decimals_0, 9);
        assert_eq!(pool.mint_decimals_1, 6);
        assert_eq!(pool.tick_spacing, tick_spacing);
        assert_eq!(pool.liquidity, liquidity);
        assert_eq!(pool.sqrt_price_x64, sqrt_price);
        assert_eq!(pool.tick_current, tick);
        // sqrt_price = 2^64 => price = 1.0
        assert!((pool.price() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_pool_state_rejects_short_data() {
        let pool_id = Pubkey::new_unique();
        assert!(parse_pool_state(&pool_id, &[0u8; 100]).is_err());
    }

    #[test]
    fn tick_array_start_index_floor() {
        let pool_id = Pubkey::new_unique();
        let data = synthetic_pool_data(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            1u128 << 64,
            0,
            125, // tick
            10,  // tick_spacing
        );
        let pool = parse_pool_state(&pool_id, &data).unwrap();
        // ticks_in_array = 60 * 10 = 600; start = (125/600)*600 = 0
        assert_eq!(pool.tick_array_start_index(), 0);
    }

    #[test]
    fn tick_array_start_index_floors_negative_tick() {
        let pool_id = Pubkey::new_unique();
        let data = synthetic_pool_data(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            1u128 << 64,
            0,
            -19234, // tick (negative)
            60,     // tick_spacing
        );
        let pool = parse_pool_state(&pool_id, &data).unwrap();
        // ticks_in_array = 60 * 60 = 3600; naive truncating division gives
        // -18000 (wrong, rounds toward zero); the correct floored start is -21600.
        assert_eq!(pool.tick_array_start_index(), -21600);
    }

    #[test]
    fn tick_array_pda_is_deterministic() {
        let pool_id = Pubkey::new_unique();
        let program_id = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let data = synthetic_pool_data(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            1u128 << 64,
            0,
            0,
            10,
        );
        let pool = parse_pool_state(&pool_id, &data).unwrap();
        let a = pool.tick_array_pda(&program_id);
        let b = pool.tick_array_pda(&program_id);
        assert_eq!(a, b);
    }

    #[test]
    fn resolve_user_ata_is_deterministic() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let a = resolve_user_ata(&owner, &mint);
        let b = resolve_user_ata(&owner, &mint);
        assert_eq!(a, b);
        // Different mint => different ATA.
        let c = resolve_user_ata(&owner, &Pubkey::new_unique());
        assert_ne!(a, c);
    }

    #[test]
    fn resolve_swap_accounts_selects_vaults_by_mint() {
        // No RPC here: build a pool and verify vault selection logic via a
        // local helper mirroring resolve_swap_accounts' mint matching.
        let mint0 = Pubkey::new_unique();
        let mint1 = Pubkey::new_unique();
        let vault0 = Pubkey::new_unique();
        let vault1 = Pubkey::new_unique();
        let pool_id = Pubkey::new_unique();
        let program_id = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let data = synthetic_pool_data(
            &Pubkey::new_unique(),
            &mint0,
            &mint1,
            &vault0,
            &vault1,
            &Pubkey::new_unique(),
            1u128 << 64,
            0,
            0,
            10,
        );
        let pool = parse_pool_state(&pool_id, &data).unwrap();

        // input = mint0 => input_vault = vault0, output_vault = vault1.
        let (iv, ov) = if &mint0 == &pool.token_mint_0 {
            (pool.token_vault_0, pool.token_vault_1)
        } else {
            (pool.token_vault_1, pool.token_vault_0)
        };
        assert_eq!(iv, vault0);
        assert_eq!(ov, vault1);

        // tick array PDA is deterministic and non-zero.
        let ta = pool.tick_array_pda(&program_id);
        assert_ne!(ta, Pubkey::default());
    }
}
