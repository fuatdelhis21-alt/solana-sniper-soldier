//! PumpSwap (pump.fun AMM) — account decoding and exact-in quotes.
//!
//! Pure, RPC-free building blocks for a **paper-only** PumpSwap path: no
//! instruction is built here and nothing is signed or sent. Layouts and math
//! follow the official pump.fun sources (IDL `pump_amm.json` in
//! `pump-fun/pump-public-docs`, and `@pump-fun/pump-swap-sdk` 2.2.0, MIT):
//!
//! - `Pool` grew by appended fields over time; a shorter account is read as
//!   if zero-padded to the current size (absent fields = 0 / false / default
//!   key), and an account shorter than the initial layout is rejected.
//! - Pricing uses the **effective quote reserves**: quote vault balance plus
//!   the signed `virtual_quote_reserves` (which is negative while fees wait in
//!   the vault). A sell is paid only from the real reserves (vault minus the
//!   waiting `protocol_fees + creator_fees`).
//! - Fees come from the pump-fees `FeeConfig`: canonical pump pools (pool
//!   creator = the pump program's `pool-authority` PDA of the base mint) pay a
//!   market-cap tier, every other pool pays the flat fees.
//!
//! Only SOL-quoted pools are supported: any other quote mint is an error, as
//! is a mayhem-mode pool, unknown data, a zero reserve or a fee >= 100 %.
//! Where the program allows a per-pool creator rate to replace the schedule's
//! rate, the higher of the two is used (costs are never underestimated).

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// PumpSwap AMM program.
pub const PUMP_AMM_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// pump.fun bonding-curve program (owner of the canonical pool authority PDA).
pub const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// pump-fees program (owner of the `FeeConfig` account).
pub const PUMP_FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
/// Wrapped SOL, the only quote mint this module prices.
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Anchor discriminator of `Pool` (IDL `accounts`).
pub const POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
/// Anchor discriminator of pump-fees `FeeConfig`.
pub const FEE_CONFIG_DISCRIMINATOR: [u8; 8] = [143, 52, 146, 187, 219, 123, 76, 155];

/// Initial `Pool` layout (up to `lp_supply`); shorter accounts are invalid.
pub const POOL_MIN_LEN: usize = 211;
/// Current serialized `Pool` size (`POOL_SIZE` in the SDK).
pub const POOL_LEN: usize = 287;
/// `FeeConfig` account lengths, one per layout version (the account length
/// selects the version, as in the SDK's `decodeFeeConfig`).
pub const FEE_CONFIG_LEN_PRE_STABLE: usize = 2512;
pub const FEE_CONFIG_LEN_POST_STABLE: usize = 4073;
pub const FEE_CONFIG_LEN_POST_EXOTIC: usize = 4097;
const FEE_TIERS_OFFSET: usize = 8 + 1 + 32 + 24;
const FEE_TIER_LEN: usize = 16 + 24;
const BPS: u128 = 10_000;

/// Decoded PumpSwap `Pool` (fields this module uses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpPool {
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    /// `Pubkey::default()` on pools that predate coin-creator fees: no
    /// creator fee is charged then.
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    /// Signed; negative while protocol/creator fees wait in the quote vault.
    pub virtual_quote_reserves: i128,
    /// Per-pool creator rate (0 = the schedule applies).
    pub creator_fee_bps: u64,
    pub protocol_fees: u64,
    pub creator_fees: u64,
}

fn pubkey_at(d: &[u8], off: usize) -> Pubkey {
    Pubkey::new_from_array(d[off..off + 32].try_into().unwrap())
}
fn u64_at(d: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(d[off..off + 8].try_into().unwrap())
}
fn u128_at(d: &[u8], off: usize) -> u128 {
    u128::from_le_bytes(d[off..off + 16].try_into().unwrap())
}
fn bool_at(d: &[u8], off: usize) -> Result<bool, String> {
    match d[off] {
        0 => Ok(false),
        1 => Ok(true),
        b => Err(format!("invalid bool byte {b} at offset {off}")),
    }
}

/// Decode a `Pool` account owned by `owner` (pure, fail-closed: wrong owner,
/// wrong discriminator, too short or an invalid bool byte are errors).
pub fn parse_pool(owner: &Pubkey, data: &[u8]) -> Result<PumpPool, String> {
    let program = Pubkey::from_str(PUMP_AMM_PROGRAM).unwrap();
    if *owner != program {
        return Err(format!("pool account is owned by {owner}, not PumpSwap"));
    }
    if data.len() < POOL_MIN_LEN {
        return Err(format!(
            "PumpSwap pool account is {} bytes; expected at least {POOL_MIN_LEN}",
            data.len()
        ));
    }
    if data[..8] != POOL_DISCRIMINATOR {
        return Err("account is not a PumpSwap Pool (discriminator)".into());
    }
    // Shorter (older) layouts read as zero-padded, as the SDK does.
    let mut d = data.to_vec();
    if d.len() < POOL_LEN {
        d.resize(POOL_LEN, 0);
    }
    Ok(PumpPool {
        creator: pubkey_at(&d, 11),
        base_mint: pubkey_at(&d, 43),
        quote_mint: pubkey_at(&d, 75),
        pool_base_token_account: pubkey_at(&d, 139),
        pool_quote_token_account: pubkey_at(&d, 171),
        coin_creator: pubkey_at(&d, 211),
        is_mayhem_mode: bool_at(&d, 243)?,
        is_cashback_coin: bool_at(&d, 244)?,
        virtual_quote_reserves: u128_at(&d, 245) as i128,
        creator_fee_bps: u64_at(&d, 261),
        protocol_fees: u64_at(&d, 271),
        creator_fees: u64_at(&d, 279),
    })
}

/// LP, protocol and creator fee rates (basis points).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fees {
    pub lp_bps: u64,
    pub protocol_bps: u64,
    pub creator_bps: u64,
}

impl Fees {
    fn read(d: &[u8], off: usize) -> Self {
        Fees {
            lp_bps: u64_at(d, off),
            protocol_bps: u64_at(d, off + 8),
            creator_bps: u64_at(d, off + 16),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeTier {
    pub market_cap_lamports_threshold: u128,
    pub fees: Fees,
}

/// pump-fees `FeeConfig` (the SOL schedule and the flat fees are what this
/// module uses; the stable schedule is decoded for completeness).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeConfig {
    pub flat_fees: Fees,
    pub fee_tiers: Vec<FeeTier>,
    pub stable_fee_tiers: Vec<FeeTier>,
    pub exotic_flat_fees: Fees,
}

fn read_tiers(d: &[u8], off: usize) -> Result<(Vec<FeeTier>, usize), String> {
    if off + 4 > d.len() {
        return Err(format!(
            "FeeConfig fee tier vector length at offset {off} runs past the account data ({} bytes)",
            d.len()
        ));
    }
    let n = u32::from_le_bytes(d[off..off + 4].try_into().unwrap()) as usize;
    let end = n
        .checked_mul(FEE_TIER_LEN)
        .and_then(|b| b.checked_add(off + 4))
        .filter(|e| *e <= d.len())
        .ok_or_else(|| {
            format!(
                "FeeConfig fee tier vector at offset {off} runs past the account data ({} bytes)",
                d.len()
            )
        })?;
    let tiers = (0..n)
        .map(|i| {
            let o = off + 4 + i * FEE_TIER_LEN;
            FeeTier {
                market_cap_lamports_threshold: u128_at(d, o),
                fees: Fees::read(d, o + 16),
            }
        })
        .collect();
    Ok((tiers, end))
}

/// Decode a pump-fees `FeeConfig` owned by `owner` (pure, fail-closed). The
/// account length selects the layout version, exactly as the SDK does.
pub fn parse_fee_config(owner: &Pubkey, data: &[u8]) -> Result<FeeConfig, String> {
    let program = Pubkey::from_str(PUMP_FEE_PROGRAM).unwrap();
    if *owner != program {
        return Err(format!("FeeConfig is owned by {owner}, not pump-fees"));
    }
    if data.len() < FEE_CONFIG_LEN_PRE_STABLE {
        return Err(format!(
            "FeeConfig account is {} bytes; expected at least {FEE_CONFIG_LEN_PRE_STABLE}",
            data.len()
        ));
    }
    if data[..8] != FEE_CONFIG_DISCRIMINATOR {
        return Err("account is not a pump-fees FeeConfig (discriminator)".into());
    }
    let flat_fees = Fees::read(data, 8 + 1 + 32);
    let (fee_tiers, end) = read_tiers(data, FEE_TIERS_OFFSET)?;
    let mut stable_fee_tiers = Vec::new();
    let mut exotic_flat_fees = Fees::default();
    if data.len() >= FEE_CONFIG_LEN_POST_STABLE {
        let (stable, end2) = read_tiers(data, end)?;
        stable_fee_tiers = stable;
        if data.len() >= FEE_CONFIG_LEN_POST_EXOTIC {
            if end2 + 24 > data.len() {
                return Err("FeeConfig exotic flat fees run past the account data".into());
            }
            exotic_flat_fees = Fees::read(data, end2);
        }
    }
    if fee_tiers.is_empty() {
        return Err("FeeConfig has no fee tiers".into());
    }
    Ok(FeeConfig {
        flat_fees,
        fee_tiers,
        stable_fee_tiers,
        exotic_flat_fees,
    })
}

/// pump-fees `FeeConfig` address of the PumpSwap program.
pub fn fee_config_address() -> Pubkey {
    let fee_program = Pubkey::from_str(PUMP_FEE_PROGRAM).unwrap();
    let amm = Pubkey::from_str(PUMP_AMM_PROGRAM).unwrap();
    Pubkey::find_program_address(&[b"fee_config", amm.as_ref()], &fee_program).0
}

/// The pump program's `pool-authority` PDA of `base_mint`: the creator of a
/// canonical (migrated) pump pool.
pub fn pump_pool_authority(base_mint: &Pubkey) -> Pubkey {
    let pump = Pubkey::from_str(PUMP_PROGRAM).unwrap();
    Pubkey::find_program_address(&[b"pool-authority", base_mint.as_ref()], &pump).0
}

/// `calculate_fee_tier` (pump-fees-math): the last tier whose threshold the
/// market cap reaches, else the first.
pub fn fee_tier(tiers: &[FeeTier], market_cap: u128) -> Option<Fees> {
    let first = tiers.first()?;
    if market_cap < first.market_cap_lamports_threshold {
        return Some(first.fees);
    }
    Some(
        tiers
            .iter()
            .rev()
            .find(|t| market_cap >= t.market_cap_lamports_threshold)
            .unwrap_or(first)
            .fees,
    )
}

/// Effective quote reserves (vault + signed virtual reserves). Errors when
/// the result is negative or does not fit a u64 (the program guarantees
/// neither happens; anything else means the data is not what we expect).
pub fn effective_quote_reserves(vault: u64, virtual_quote: i128) -> Result<u64, String> {
    (vault as i128)
        .checked_add(virtual_quote)
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| "effective quote reserves out of range".to_string())
}

/// Fee rates a trade on `pool` pays (SOL-quoted pools only).
/// `base_supply` is the base mint's supply; reserves are raw base units and
/// effective quote reserves (lamports).
pub fn pool_fees(
    pool: &PumpPool,
    config: &FeeConfig,
    base_supply: u64,
    base_reserve: u64,
    effective_quote: u64,
) -> Result<Fees, String> {
    if pool.quote_mint != Pubkey::from_str(WSOL_MINT).unwrap() {
        return Err(format!(
            "PumpSwap pool quote mint {} is not wSOL (unsupported)",
            pool.quote_mint
        ));
    }
    if pool.is_mayhem_mode {
        return Err("PumpSwap mayhem-mode pool (unsupported)".into());
    }
    if base_reserve == 0 {
        return Err("PumpSwap pool has no base reserve".into());
    }
    let mut fees = if pool.creator == pump_pool_authority(&pool.base_mint) {
        let market_cap = (effective_quote as u128)
            .checked_mul(base_supply as u128)
            .ok_or("market cap overflow")?
            / base_reserve as u128;
        fee_tier(&config.fee_tiers, market_cap).ok_or("FeeConfig has no fee tiers")?
    } else {
        config.flat_fees
    };
    // The program may replace the schedule's creator rate with the pool's
    // own; we do not read whether that gate is on, so take the higher one.
    if pool.creator_fee_bps > 0 {
        fees.creator_bps = fees.creator_bps.max(pool.creator_fee_bps);
    }
    if pool.coin_creator == Pubkey::default() {
        fees.creator_bps = 0;
    }
    let total = fees.lp_bps as u128 + fees.protocol_bps as u128 + fees.creator_bps as u128;
    if total >= BPS {
        return Err(format!(
            "PumpSwap fees of {total} bps are not a valid trade"
        ));
    }
    Ok(fees)
}

/// `fee()` of the SDK: ceil(amount * bps / 10 000).
fn fee(amount: u128, bps: u64) -> u128 {
    (amount * bps as u128).div_ceil(BPS)
}

/// Spend exactly `quote_in` lamports (fees included) on base tokens:
/// `buy_exact_quote_in` / SDK `buyQuoteInput`. Returns the base tokens out.
pub fn buy_exact_quote_in(
    quote_in: u64,
    base_reserve: u64,
    effective_quote: u64,
    fees: Fees,
) -> Result<u64, String> {
    if base_reserve == 0 || effective_quote == 0 {
        return Err("PumpSwap pool reserves cannot be zero".into());
    }
    let q = quote_in as u128;
    let total_bps = fees.lp_bps as u128 + fees.protocol_bps as u128 + fees.creator_bps as u128;
    let mut net = q * BPS / (BPS + total_bps);
    let with_fees =
        net + fee(net, fees.lp_bps) + fee(net, fees.protocol_bps) + fee(net, fees.creator_bps);
    if with_fees > q {
        net -= with_fees - q;
    }
    if net == 0 {
        return Err("the quote amount does not cover the fees".into());
    }
    let input = net - 1;
    let out = base_reserve as u128 * input / (effective_quote as u128 + input);
    u64::try_from(out).map_err(|_| "base output out of range".into())
}

/// Sell exactly `base_in` base tokens for lamports: SDK `sellBaseInput`.
/// Returns the lamports the seller receives after fees. `fee_buckets` is
/// `protocol_fees + creator_fees` waiting in the vault (sells are paid only
/// from the real reserves).
pub fn sell_exact_base_in(
    base_in: u64,
    base_reserve: u64,
    quote_vault: u64,
    virtual_quote: i128,
    fee_buckets: u64,
    fees: Fees,
) -> Result<u64, String> {
    if base_reserve == 0 || quote_vault == 0 {
        return Err("PumpSwap pool reserves cannot be zero".into());
    }
    let eff = effective_quote_reserves(quote_vault, virtual_quote)? as u128;
    let b = base_in as u128;
    let gross = eff * b / (base_reserve as u128 + b);
    let lp = fee(gross, fees.lp_bps);
    let without_lp = gross.checked_sub(lp).ok_or("fees exceed the output")?;
    let real = (quote_vault as u128)
        .checked_sub(fee_buckets as u128)
        .ok_or("fee buckets exceed the quote vault")?;
    if real < without_lp {
        return Err("insufficient real quote reserves for the sell".into());
    }
    let out = without_lp
        .checked_sub(fee(gross, fees.creator_bps))
        .and_then(|v| v.checked_sub(fee(gross, fees.protocol_bps)))
        .ok_or("fees exceed the output")?;
    u64::try_from(out).map_err(|_| "quote output out of range".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn fee_program() -> Pubkey {
        Pubkey::from_str(PUMP_FEE_PROGRAM).unwrap()
    }

    fn mainnet_fee_config() -> FeeConfig {
        let b64 = include_str!("fixtures/pumpswap_fee_config_mainnet.b64").trim();
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(data.len(), FEE_CONFIG_LEN_POST_STABLE);
        parse_fee_config(&fee_program(), &data).unwrap()
    }

    fn fees(lp: u64, protocol: u64, creator: u64) -> Fees {
        Fees {
            lp_bps: lp,
            protocol_bps: protocol,
            creator_bps: creator,
        }
    }

    /// A SOL pool with `creator` and `coin_creator`.
    fn pool(base_mint: Pubkey, creator: Pubkey, coin_creator: Pubkey) -> PumpPool {
        PumpPool {
            creator,
            base_mint,
            quote_mint: Pubkey::from_str(WSOL_MINT).unwrap(),
            pool_base_token_account: Pubkey::new_unique(),
            pool_quote_token_account: Pubkey::new_unique(),
            coin_creator,
            is_mayhem_mode: false,
            is_cashback_coin: false,
            virtual_quote_reserves: 0,
            creator_fee_bps: 0,
            protocol_fees: 0,
            creator_fees: 0,
        }
    }

    #[test]
    fn decodes_the_live_mainnet_fee_config() {
        // Fixture: @pump-fun/pump-swap-sdk 2.2.0 (MIT), account
        // 5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx at slot 445186127.
        assert_eq!(
            fee_config_address().to_string(),
            "5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx"
        );
        let c = mainnet_fee_config();
        assert_eq!(c.flat_fees, fees(25, 5, 0));
        assert_eq!(c.fee_tiers.len(), 25);
        assert_eq!(c.stable_fee_tiers.len(), 25);
        assert_eq!(c.exotic_flat_fees, Fees::default());
        // First rows as pinned by the SDK's fixture.
        assert_eq!(c.fee_tiers[0].market_cap_lamports_threshold, 0);
        assert_eq!(c.fee_tiers[0].fees, fees(2, 93, 30));
        assert_eq!(
            c.fee_tiers[1].market_cap_lamports_threshold,
            420_000_000_000
        );
        assert_eq!(c.fee_tiers[1].fees, fees(20, 5, 95));
        assert_eq!(
            c.stable_fee_tiers[1].market_cap_lamports_threshold,
            59_000_000_000
        );
    }

    #[test]
    fn fee_config_rejects_bad_accounts() {
        let b64 = include_str!("fixtures/pumpswap_fee_config_mainnet.b64").trim();
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert!(
            parse_fee_config(&Pubkey::new_unique(), &data).is_err(),
            "owner"
        );
        let mut bad = data.clone();
        bad[0] ^= 0xff;
        assert!(parse_fee_config(&fee_program(), &bad)
            .unwrap_err()
            .contains("discriminator"));
        assert!(parse_fee_config(&fee_program(), &data[..FEE_CONFIG_LEN_PRE_STABLE - 1]).is_err());
        let mut long_vec = data.clone();
        long_vec[FEE_TIERS_OFFSET..FEE_TIERS_OFFSET + 4].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse_fee_config(&fee_program(), &long_vec)
            .unwrap_err()
            .contains("runs past"));
        // A pre-stable length ignores the stable vector (account length
        // selects the version).
        let pre = parse_fee_config(&fee_program(), &data[..FEE_CONFIG_LEN_PRE_STABLE]).unwrap();
        assert!(pre.stable_fee_tiers.is_empty());
        assert_eq!(pre.fee_tiers.len(), 25);
    }

    #[test]
    fn fee_tier_picks_the_last_reached_threshold() {
        let t = |th: u128, lp: u64| FeeTier {
            market_cap_lamports_threshold: th,
            fees: fees(lp, 0, 0),
        };
        let tiers = [t(100, 1), t(200, 2), t(300, 3)];
        assert_eq!(fee_tier(&tiers, 50).unwrap().lp_bps, 1);
        assert_eq!(fee_tier(&tiers, 200).unwrap().lp_bps, 2);
        assert_eq!(fee_tier(&tiers, 299).unwrap().lp_bps, 2);
        assert_eq!(fee_tier(&tiers, 10_000).unwrap().lp_bps, 3);
        assert!(fee_tier(&[], 1).is_none());
    }

    /// SDK goldens ("SOL pools keep their pre-change amounts (live mainnet
    /// FeeConfig goldens)" in buy.spec.ts / sell.spec.ts): supply 1e15, base
    /// reserve 5e14, canonical pump pool.
    #[test]
    fn matches_the_sdk_mainnet_goldens() {
        let config = mainnet_fee_config();
        let mint = Pubkey::new_unique();
        let canonical = pump_pool_authority(&mint);
        let supply = 1_000_000_000_000_000u64;
        let base_reserve = 500_000_000_000_000u64;
        let with_creator = pool(mint, canonical, Pubkey::new_unique());
        let no_creator = pool(mint, canonical, Pubkey::default());
        let flat = pool(mint, Pubkey::new_unique(), Pubkey::new_unique());

        let buy = |p: &PumpPool, quote_vault: u64, virt: i128, quote_in: u64| {
            let eff = effective_quote_reserves(quote_vault, virt).unwrap();
            let f = pool_fees(p, &config, supply, base_reserve, eff).unwrap();
            buy_exact_quote_in(quote_in, base_reserve, eff, f).unwrap()
        };
        assert_eq!(
            buy(&no_creator, 250_000_000_000, 0, 1_000_000_000),
            1_987_083_949_507
        );
        assert_eq!(
            buy(
                &with_creator,
                10_000_000_000_000,
                30_000_000_000,
                50_000_000_000
            ),
            2_459_357_881_156
        );

        let sell = |p: &PumpPool, quote_vault: u64, virt: i128, base_in: u64| {
            let eff = effective_quote_reserves(quote_vault, virt).unwrap();
            let f = pool_fees(p, &config, supply, base_reserve, eff).unwrap();
            sell_exact_base_in(base_in, base_reserve, quote_vault, virt, 0, f).unwrap()
        };
        assert_eq!(
            sell(&with_creator, 50_000_000_000, 0, 1_000_000_000_000),
            98_552_892
        );
        assert_eq!(
            sell(
                &no_creator,
                1_000_000_000_000,
                30_000_000_000,
                10_000_000_000_000
            ),
            20_145_588_234
        );
        // Non-pump pool: flat fees.
        assert_eq!(
            sell(&flat, 50_000_000_000, 0, 1_000_000_000_000),
            99_500_997
        );
    }

    #[test]
    fn sdk_typical_sell_with_global_config_fees() {
        // sell.spec.ts "typical inputs": 30 + 20 bps, no creator fee.
        let out = sell_exact_base_in(50_000, 1_000_000, 2_000_000, 0, 0, fees(30, 20, 0)).unwrap();
        assert_eq!(out, 94_761);
    }

    #[test]
    fn fees_fail_closed_and_creator_rate_is_never_underestimated() {
        let config = mainnet_fee_config();
        let mint = Pubkey::new_unique();
        let canonical = pump_pool_authority(&mint);
        let p = pool(mint, canonical, Pubkey::new_unique());
        // Low market cap -> first tier: 2 + 93 + 30 bps.
        assert_eq!(
            pool_fees(&p, &config, 1_000_000, 1_000_000, 1).unwrap(),
            fees(2, 93, 30)
        );
        let mut higher = p.clone();
        higher.creator_fee_bps = 250;
        assert_eq!(
            pool_fees(&higher, &config, 1_000_000, 1_000_000, 1)
                .unwrap()
                .creator_bps,
            250
        );
        let mut lower = p.clone();
        lower.creator_fee_bps = 1;
        assert_eq!(
            pool_fees(&lower, &config, 1_000_000, 1_000_000, 1)
                .unwrap()
                .creator_bps,
            30
        );

        let mut usdc = p.clone();
        usdc.quote_mint = Pubkey::new_unique();
        assert!(pool_fees(&usdc, &config, 1, 1, 1)
            .unwrap_err()
            .contains("not wSOL"));
        let mut mayhem = p.clone();
        mayhem.is_mayhem_mode = true;
        assert!(pool_fees(&mayhem, &config, 1, 1, 1).is_err());
        assert!(
            pool_fees(&p, &config, 1, 0, 1).is_err(),
            "zero base reserve"
        );
        let mut absurd = config.clone();
        absurd.flat_fees = fees(9_000, 1_000, 0);
        let flat = pool(mint, Pubkey::new_unique(), Pubkey::default());
        assert!(pool_fees(&flat, &absurd, 1, 1, 1).is_err(), "fee >= 100 %");
    }

    #[test]
    fn quotes_fail_closed() {
        assert!(buy_exact_quote_in(1_000, 0, 1, Fees::default()).is_err());
        assert!(buy_exact_quote_in(1_000, 1, 0, Fees::default()).is_err());
        assert!(buy_exact_quote_in(0, 1_000, 1_000, Fees::default()).is_err());
        assert!(sell_exact_base_in(10, 0, 1, 0, 0, Fees::default()).is_err());
        assert!(effective_quote_reserves(10, -11).is_err());
        assert!(effective_quote_reserves(u64::MAX, 1).is_err());
        assert_eq!(effective_quote_reserves(100, -30).unwrap(), 70);
        // Waiting fees are not sell liquidity.
        assert!(
            sell_exact_base_in(1_000_000, 1_000, 100, 0, 95, Fees::default())
                .unwrap_err()
                .contains("real quote reserves")
        );
        assert!(sell_exact_base_in(1, 1, 1, 0, 2, Fees::default()).is_err());
    }

    #[test]
    fn pool_layouts_by_length() {
        let program = Pubkey::from_str(PUMP_AMM_PROGRAM).unwrap();
        let (creator, base, quote, coin) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::from_str(WSOL_MINT).unwrap(),
            Pubkey::new_unique(),
        );
        let mut d = vec![0u8; POOL_LEN];
        d[..8].copy_from_slice(&POOL_DISCRIMINATOR);
        d[11..43].copy_from_slice(creator.as_ref());
        d[43..75].copy_from_slice(base.as_ref());
        d[75..107].copy_from_slice(quote.as_ref());
        d[211..243].copy_from_slice(coin.as_ref());
        d[243] = 0;
        d[244] = 1;
        d[245..261].copy_from_slice(&(-42i128).to_le_bytes());
        d[261..269].copy_from_slice(&250u64.to_le_bytes());
        d[271..279].copy_from_slice(&11u64.to_le_bytes());
        d[279..287].copy_from_slice(&13u64.to_le_bytes());

        let full = parse_pool(&program, &d).unwrap();
        assert_eq!(
            (full.creator, full.base_mint, full.quote_mint),
            (creator, base, quote)
        );
        assert_eq!(full.coin_creator, coin);
        assert!(full.is_cashback_coin && !full.is_mayhem_mode);
        assert_eq!(full.virtual_quote_reserves, -42);
        assert_eq!(
            (full.creator_fee_bps, full.protocol_fees, full.creator_fees),
            (250, 11, 13)
        );
        // Extended (longer) accounts decode the same.
        let mut ext = d.clone();
        ext.resize(301, 0);
        assert_eq!(parse_pool(&program, &ext).unwrap(), full);
        // 261 bytes: no creator fee / fee buckets yet.
        let p261 = parse_pool(&program, &d[..261]).unwrap();
        assert_eq!(p261.virtual_quote_reserves, -42);
        assert_eq!((p261.creator_fee_bps, p261.protocol_fees), (0, 0));
        // 211 bytes (initial layout): no coin creator, no virtual reserves.
        let p211 = parse_pool(&program, &d[..211]).unwrap();
        assert_eq!(p211.coin_creator, Pubkey::default());
        assert_eq!(p211.virtual_quote_reserves, 0);

        assert!(parse_pool(&program, &d[..210]).is_err(), "too short");
        assert!(parse_pool(&Pubkey::new_unique(), &d).is_err(), "owner");
        let mut disc = d.clone();
        disc[0] ^= 1;
        assert!(parse_pool(&program, &disc).is_err(), "discriminator");
        let mut bad_bool = d.clone();
        bad_bool[243] = 7;
        assert!(parse_pool(&program, &bad_bool).is_err(), "bool byte");
    }
}
