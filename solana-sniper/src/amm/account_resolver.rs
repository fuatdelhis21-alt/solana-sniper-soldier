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
pub const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
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
    /// Pool trade fee from its AmmConfig, in parts per million (2500 =
    /// 0.25%). `None` until `resolve_swap_accounts` reads the AmmConfig.
    pub trade_fee_rate: Option<u32>,
    /// `PoolState.tick_array_bitmap` (1024 bits: initialized tick arrays
    /// with start index in [-512, 511] arrays). `None` for a short buffer.
    pub tick_array_bitmap: Option<[u64; 16]>,
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
        self.tick_array_pda_at(program_id, self.tick_array_start_index())
    }

    /// Tick-array PDAs to try for a swap, starting with the array holding
    /// the current tick and walking in the price direction of the swap
    /// (`zero_for_one` = selling token0 => price and tick go DOWN).
    pub fn tick_array_candidates(
        &self,
        program_id: &Pubkey,
        zero_for_one: bool,
        count: i32,
    ) -> Vec<Pubkey> {
        let size = TICK_ARRAY_SIZE * i32::from(self.tick_spacing);
        let start = self.tick_array_start_index();
        let step = if zero_for_one { -size } else { size };
        (0..count)
            .map(|k| self.tick_array_pda_at(program_id, start + k * step))
            .collect()
    }

    /// Tick-array PDA for an explicit start index.
    pub fn tick_array_pda_at(&self, program_id: &Pubkey, start: i32) -> Pubkey {
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
        trade_fee_rate: None,
        tick_array_bitmap: parse_tick_array_bitmap(data),
    })
}

/// Byte offset of `PoolState.tick_array_bitmap` ([u64; 16]) in the packed
/// account: 8 discriminator + bump + 7 pubkeys + decimals/tick spacing +
/// liquidity/sqrt price/tick + paddings + fee growth / protocol fees / swap
/// amounts + status + padding (= 397) + 3 RewardInfo of 169 bytes.
pub const TICK_ARRAY_BITMAP_OFFSET: usize = 904;

fn parse_tick_array_bitmap(data: &[u8]) -> Option<[u64; 16]> {
    let raw = data.get(TICK_ARRAY_BITMAP_OFFSET..TICK_ARRAY_BITMAP_OFFSET + 128)?;
    let mut words = [0u64; 16];
    for (i, w) in words.iter_mut().enumerate() {
        *w = u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().ok()?);
    }
    Some(words)
}

/// Start indexes of the initialized tick arrays in the swap direction,
/// beginning with the array that holds the current tick (inclusive), read
/// from the pool's own bitmap — how Raydium itself finds the "first
/// initialized tick array". Needed for new pools whose liquidity sits in a
/// wide range: the next initialized array can be hundreds of arrays away,
/// far beyond a fixed scan. Only the default bitmap range (±512 arrays) is
/// covered; beyond it the caller falls back to a PDA scan.
pub fn bitmap_initialized_starts(
    bitmap: &[u64; 16],
    tick_spacing: u16,
    current_start: i32,
    zero_for_one: bool,
    max: usize,
) -> Vec<i32> {
    let size = TICK_ARRAY_SIZE * i32::from(tick_spacing.max(1));
    let bit_of = |start: i32| start.div_euclid(size) + DEFAULT_BITMAP_ARRAYS;
    let is_set = |bit: i32| (bitmap[(bit / 64) as usize] >> (bit % 64)) & 1 == 1;
    let mut out = Vec::new();
    let mut bit = bit_of(current_start);
    let step = if zero_for_one { -1 } else { 1 };
    while (0..2 * DEFAULT_BITMAP_ARRAYS).contains(&bit) && out.len() < max {
        if is_set(bit) {
            out.push((bit - DEFAULT_BITMAP_ARRAYS) * size);
        }
        bit += step;
    }
    out
}

/// `AmmConfig.trade_fee_rate` (u32 LE, parts per million) — layout:
/// 8 discriminator + bump u8 + index u16 + owner Pubkey + protocol_fee_rate
/// u32, so the trade fee starts at byte 47.
pub fn parse_trade_fee_rate(data: &[u8]) -> Result<u32, String> {
    let bytes: [u8; 4] = data
        .get(47..51)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("AmmConfig data too short ({} bytes)", data.len()))?;
    let rate = u32::from_le_bytes(bytes);
    // Raydium fee tiers are 100..=10_000 ppm (0.01%..1%); anything outside
    // a sane range means a wrong account/layout — fail closed.
    if rate == 0 || rate > 100_000 {
        return Err(format!("implausible AmmConfig trade_fee_rate {rate}"));
    }
    Ok(rate)
}

/// Seed of the pool's TickArrayBitmapExtension PDA.
pub const BITMAP_EXTENSION_SEED: &[u8] = b"pool_tick_array_bitmap_extension";
/// The pool state's built-in bitmap tracks tick arrays whose start index is
/// within ±512 arrays of tick 0; beyond that Raydium needs the extension.
pub const DEFAULT_BITMAP_ARRAYS: i32 = 512;

/// The pool's tick-array bitmap extension PDA.
pub fn bitmap_extension_pda(pool_id: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[BITMAP_EXTENSION_SEED, pool_id.as_ref()], program_id).0
}

/// True when a swap scanning `scan` arrays from the current tick could
/// leave the range of the pool's built-in bitmap (so the extension account
/// must be supplied). Normal pools never get close to this.
pub fn needs_bitmap_extension(tick_current: i32, tick_spacing: u16, scan: i32) -> bool {
    let size = i64::from(TICK_ARRAY_SIZE) * i64::from(tick_spacing);
    let limit = i64::from(DEFAULT_BITMAP_ARRAYS) * size;
    i64::from(tick_current).abs() + i64::from(scan + 1) * size >= limit
}

/// Maximum tick arrays passed to a swap (the account in the instruction
/// plus remaining accounts) and how far to scan for initialized ones.
pub const MAX_SWAP_TICK_ARRAYS: usize = 3;
pub const TICK_ARRAY_SCAN: i32 = 10;

/// Pick the tick arrays for a swap: the candidates (in swap direction) that
/// exist on-chain and are owned by the CLMM program, first `MAX` of them.
/// Raydium requires the first *initialized* array in the swap direction as
/// the instruction's tick-array account; an uninitialized PDA (the array of
/// the current tick can be empty) makes the swap fail on-chain.
pub fn select_tick_arrays(
    candidates: &[Pubkey],
    owners: &[Option<Pubkey>],
    program_id: &Pubkey,
) -> Vec<Pubkey> {
    candidates
        .iter()
        .zip(owners.iter())
        .filter(|(_, owner)| owner.as_ref() == Some(program_id))
        .map(|(pk, _)| *pk)
        .take(MAX_SWAP_TICK_ARRAYS)
        .collect()
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
/// Anchor account discriminator: sha256("account:<Name>")[..8].
pub fn anchor_account_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("account:{name}").as_bytes());
    let mut d = [0u8; 8];
    d.copy_from_slice(&h[..8]);
    d
}

/// Fail-closed identity check of a CLMM program account: it must be owned
/// by `program_id` and carry the Anchor discriminator of `kind` (e.g.
/// "PoolState", "AmmConfig"). Without it any account with a matching byte
/// layout (another program's, or a wrong cluster's) would be parsed and fed
/// into the price / liquidity / fee data the risk gates rely on.
pub fn verify_clmm_account(
    kind: &str,
    key: &Pubkey,
    owner: &Pubkey,
    data: &[u8],
    program_id: &Pubkey,
) -> Result<(), String> {
    if owner != program_id {
        return Err(format!(
            "{kind} {key} is owned by {owner}, not the CLMM program {program_id} (fail-closed)"
        ));
    }
    if data.get(..8) != Some(&anchor_account_discriminator(kind)[..]) {
        return Err(format!(
            "{key} is not a CLMM {kind} account (discriminator mismatch, fail-closed)"
        ));
    }
    Ok(())
}

/// Fetch, verify (owner + discriminator) and parse a CLMM pool state.
pub fn fetch_pool_state(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    program_id: &Pubkey,
) -> Result<ResolvedPool, String> {
    let account = rpc
        .get_account(pool_id)
        .map_err(|e| format!("failed to fetch pool state {pool_id}: {e}"))?;
    verify_clmm_account(
        "PoolState",
        pool_id,
        &account.owner,
        &account.data,
        program_id,
    )?;
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
    let mut pool = fetch_pool_state(rpc, pool_id, program_id)?;

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

    // One RPC round trip for the AmmConfig (real fee tier) and the
    // tick-array candidates in the swap direction.
    let zero_for_one = input_mint == &pool.token_mint_0;
    // The pool's bitmap names the initialized arrays in the swap direction;
    // a fixed scan around the current tick is the fallback (bitmap missing,
    // or liquidity only beyond the default bitmap range).
    let from_bitmap: Vec<Pubkey> = pool
        .tick_array_bitmap
        .map(|b| {
            bitmap_initialized_starts(
                &b,
                pool.tick_spacing,
                pool.tick_array_start_index(),
                zero_for_one,
                MAX_SWAP_TICK_ARRAYS,
            )
        })
        .unwrap_or_default()
        .into_iter()
        .map(|start| pool.tick_array_pda_at(program_id, start))
        .collect();
    let candidates = if from_bitmap.is_empty() {
        pool.tick_array_candidates(program_id, zero_for_one, TICK_ARRAY_SCAN)
    } else {
        from_bitmap
    };
    let ext_needed = needs_bitmap_extension(pool.tick_current, pool.tick_spacing, TICK_ARRAY_SCAN);
    let ext_pda = bitmap_extension_pda(pool_id, program_id);
    let mut keys = Vec::with_capacity(candidates.len() + 2);
    keys.push(pool.amm_config);
    keys.extend_from_slice(&candidates);
    if ext_needed {
        keys.push(ext_pda);
    }
    let fetched = rpc
        .get_multiple_accounts(&keys)
        .map_err(|e| format!("failed to fetch amm config / tick arrays for {pool_id}: {e}"))?;
    let amm_config = fetched
        .first()
        .and_then(|a| a.as_ref())
        .ok_or_else(|| format!("amm config {} not found", pool.amm_config))?;
    verify_clmm_account(
        "AmmConfig",
        &pool.amm_config,
        &amm_config.owner,
        &amm_config.data,
        program_id,
    )?;
    pool.trade_fee_rate = Some(parse_trade_fee_rate(&amm_config.data)?);
    let bitmap_extension = if ext_needed {
        match fetched.get(1 + candidates.len()).and_then(|a| a.as_ref()) {
            Some(acc) if &acc.owner == program_id => Some(ext_pda),
            _ => {
                return Err(format!(
                    "pool {pool_id} trades near the edge of its tick bitmap but has no bitmap extension account (fail-closed)"
                ))
            }
        }
    } else {
        None
    };
    let owners: Vec<Option<Pubkey>> = fetched[1..1 + candidates.len()]
        .iter()
        .map(|a| a.as_ref().map(|acc| acc.owner))
        .collect();
    let arrays = select_tick_arrays(&candidates, &owners, program_id);
    let Some((&tick_array, extra)) = arrays.split_first() else {
        return Err(format!(
            "no initialized tick array in the swap direction for pool {pool_id} (tick {}, spacing {}, bitmap {}) (fail-closed)",
            pool.tick_current,
            pool.tick_spacing,
            if pool.tick_array_bitmap.is_some() { "read" } else { "missing" }
        ));
    };

    let accounts = SwapAccounts {
        amm_config: pool.amm_config,
        pool_state: *pool_id,
        input_token_account,
        output_token_account,
        input_vault,
        output_vault,
        observation_state: pool.observation_key,
        tick_array,
        extra_tick_arrays: extra.to_vec(),
        bitmap_extension,
    };

    Ok((accounts, pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap_with(starts: &[i32], size: i32) -> [u64; 16] {
        let mut b = [0u64; 16];
        for s in starts {
            let bit = s.div_euclid(size) + DEFAULT_BITMAP_ARRAYS;
            b[(bit / 64) as usize] |= 1 << (bit % 64);
        }
        b
    }

    #[test]
    fn bitmap_finds_far_initialized_arrays_in_swap_direction() {
        let spacing = 60u16;
        let size = 60 * 60;
        // Launch-style pool: current tick deep negative, liquidity in a
        // wide range whose arrays are far away on both sides.
        let cur = -100 * size;
        let b = bitmap_with(&[-300 * size, -100 * size, 250 * size, 400 * size], size);
        // Price up (one_for_zero): current array first, then the far ones.
        assert_eq!(
            bitmap_initialized_starts(&b, spacing, cur, false, 3),
            vec![-100 * size, 250 * size, 400 * size]
        );
        // Price down (zero_for_one).
        assert_eq!(
            bitmap_initialized_starts(&b, spacing, cur, true, 3),
            vec![-100 * size, -300 * size]
        );
        // Current array empty: the first hit is 350 arrays away (beyond
        // the old 10-array scan).
        let b = bitmap_with(&[250 * size], size);
        assert_eq!(
            bitmap_initialized_starts(&b, spacing, cur, false, 3),
            vec![250 * size]
        );
        assert!(bitmap_initialized_starts(&b, spacing, cur, true, 3).is_empty());
        // Range edges and an empty bitmap.
        let b = bitmap_with(&[-512 * size, 511 * size], size);
        assert_eq!(
            bitmap_initialized_starts(&b, spacing, 0, true, 3),
            vec![-512 * size]
        );
        assert_eq!(
            bitmap_initialized_starts(&b, spacing, 0, false, 3),
            vec![511 * size]
        );
        assert!(bitmap_initialized_starts(&[0; 16], spacing, 0, false, 3).is_empty());
    }

    #[test]
    fn tick_array_bitmap_is_read_at_its_packed_offset() {
        let mut data = vec![0u8; 1544];
        data[TICK_ARRAY_BITMAP_OFFSET] = 0b101; // word 0, bits 0 and 2
        data[TICK_ARRAY_BITMAP_OFFSET + 15 * 8 + 7] = 0x80; // word 15, bit 63
        let b = parse_tick_array_bitmap(&data).unwrap();
        assert_eq!(b[0], 0b101);
        assert_eq!(b[15], 1 << 63);
        assert!(parse_tick_array_bitmap(&data[..1000]).is_none());
        // Offset = 397 + 3 * RewardInfo(169).
        assert_eq!(TICK_ARRAY_BITMAP_OFFSET, 397 + 3 * 169);
    }

    #[test]
    fn clmm_account_discriminators_match_anchor() {
        // sha256("account:PoolState")[..8] / sha256("account:AmmConfig")[..8]
        // as published in the Raydium CLMM IDL.
        assert_eq!(
            anchor_account_discriminator("PoolState"),
            [247, 237, 227, 245, 215, 195, 222, 70]
        );
        assert_eq!(
            anchor_account_discriminator("AmmConfig"),
            [218, 244, 33, 104, 203, 203, 43, 111]
        );
    }

    #[test]
    fn verify_clmm_account_is_fail_closed() {
        let program = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let key = Pubkey::new_unique();
        let mut data = anchor_account_discriminator("PoolState").to_vec();
        data.extend_from_slice(&[0u8; 300]);
        assert!(verify_clmm_account("PoolState", &key, &program, &data, &program).is_ok());
        // Wrong owner (e.g. a look-alike account of another program).
        let other = Pubkey::new_unique();
        assert!(verify_clmm_account("PoolState", &key, &other, &data, &program).is_err());
        // Right owner, wrong account type.
        assert!(verify_clmm_account("AmmConfig", &key, &program, &data, &program).is_err());
        // Too short.
        assert!(verify_clmm_account("PoolState", &key, &program, &data[..4], &program).is_err());
    }

    /// Build a synthetic 273-byte pool state buffer with known field values.
    #[allow(clippy::too_many_arguments)]
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
        let (iv, ov) = if mint0 == pool.token_mint_0 {
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

    #[test]
    fn tick_array_candidates_walk_in_swap_direction() {
        let program_id = Pubkey::new_unique();
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
            -130,
            10,
        );
        let pool = parse_pool_state(&pool_id, &data).unwrap();
        let size = TICK_ARRAY_SIZE * i32::from(pool.tick_spacing);
        let start = pool.tick_array_start_index();
        let down = pool.tick_array_candidates(&program_id, true, 3);
        let up = pool.tick_array_candidates(&program_id, false, 3);
        assert_eq!(down[0], up[0], "both start at the current tick's array");
        assert_eq!(down[1], pool.tick_array_pda_at(&program_id, start - size));
        assert_eq!(up[1], pool.tick_array_pda_at(&program_id, start + size));
        assert_eq!(
            down[2],
            pool.tick_array_pda_at(&program_id, start - 2 * size)
        );
    }

    #[test]
    fn select_tick_arrays_skips_uninitialized_and_caps() {
        let program_id = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let c: Vec<Pubkey> = (0..6).map(|_| Pubkey::new_unique()).collect();
        // current array missing, then a foreign-owned account, then 4 valid
        let owners = vec![
            None,
            Some(other),
            Some(program_id),
            Some(program_id),
            Some(program_id),
            Some(program_id),
        ];
        let sel = select_tick_arrays(&c, &owners, &program_id);
        assert_eq!(sel, vec![c[2], c[3], c[4]]);
        assert!(select_tick_arrays(&c, &[None; 6], &program_id).is_empty());
    }

    #[test]
    fn bitmap_extension_only_needed_near_the_bitmap_edge() {
        // spacing 60: built-in bitmap covers |tick| < 512*60*60 = 1_843_200,
        // beyond Solana's max tick (443_636) — never needed.
        assert!(!needs_bitmap_extension(443_636, 60, TICK_ARRAY_SCAN));
        // spacing 1: covers |tick| < 30_720.
        assert!(!needs_bitmap_extension(0, 1, TICK_ARRAY_SCAN));
        assert!(!needs_bitmap_extension(-20_000, 1, TICK_ARRAY_SCAN));
        assert!(needs_bitmap_extension(30_100, 1, TICK_ARRAY_SCAN));
        assert!(needs_bitmap_extension(-40_000, 1, TICK_ARRAY_SCAN));
        let pool = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        assert_eq!(
            bitmap_extension_pda(&pool, &program),
            bitmap_extension_pda(&pool, &program)
        );
    }

    #[test]
    fn trade_fee_rate_parses_amm_config_layout() {
        let mut data = vec![0u8; 117];
        data[47..51].copy_from_slice(&2500u32.to_le_bytes());
        assert_eq!(parse_trade_fee_rate(&data).unwrap(), 2500);
        assert!(parse_trade_fee_rate(&data[..40]).is_err());
        let zero = vec![0u8; 117];
        assert!(parse_trade_fee_rate(&zero).is_err());
    }
}
