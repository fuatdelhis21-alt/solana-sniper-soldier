//! PumpSwap swap instructions, for **simulation only** (`--dry-run`).
//!
//! Builds the `buy` / `sell` instructions of the PumpSwap program with the
//! account list, remaining accounts and user-side setup that the official
//! `@pump-fun/pump-swap-sdk` 2.2.0 builds (`buyInstructionsNoPool`,
//! `sellInstructionsNoPool`, `withSwapUserAccounts`, `swapAccounts`),
//! account order from the IDL `pump_amm.json`. Nothing here signs or sends:
//! the caller only passes the transaction to `simulateTransaction`. Live
//! trading on PumpSwap is not implemented.
//!
//! Choices that differ from the SDK and why:
//! - The protocol and buyback fee recipients are the first non-default
//!   entries of `GlobalConfig` (the SDK picks one at random; any listed
//!   recipient is accepted by the program).
//! - The swap output stays in the user's token account (no unwrap of the
//!   wSOL output), so a simulation can read the output amount.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::pumpswap::{PumpPool, PUMP_AMM_PROGRAM, PUMP_FEE_PROGRAM, WSOL_MINT};

pub const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

/// Anchor discriminators (IDL `instructions[].discriminator`); the tests
/// check them against sha256("global:<name>").
pub const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
pub const EXTEND_ACCOUNT_DISCRIMINATOR: [u8; 8] = [234, 102, 194, 203, 150, 72, 62, 229];
/// `GlobalConfig` account discriminator (IDL `accounts`).
pub const GLOBAL_CONFIG_DISCRIMINATOR: [u8; 8] = [149, 8, 156, 202, 160, 252, 176, 217];
/// Pools shorter than this are extended before a trade (SDK
/// `withFixPoolInstructions`, `POOL_ACCOUNT_NEW_SIZE`).
pub const POOL_ACCOUNT_NEW_SIZE: usize = 300;

/// `GlobalConfig` byte offsets (IDL field order, Borsh, after the 8-byte
/// discriminator): admin 8..40, lp bps 40..48, protocol bps 48..56,
/// disable_flags 56, protocol_fee_recipients 57..313 (8 × 32), coin creator
/// bps 313..321, admin_set_coin_creator_authority 321..353, whitelist_pda
/// 353..385, reserved_fee_recipient 385..417, mayhem_mode_enabled 417,
/// reserved_fee_recipients 418..642 (7 × 32), is_cashback_enabled 642,
/// buyback_fee_recipients 643..899 (8 × 32), buyback bps 899..907.
const GC_DISABLE_FLAGS: usize = 56;
const GC_PROTOCOL_RECIPIENTS: usize = 57;
const GC_BUYBACK_RECIPIENTS: usize = 643;
const GC_MIN_LEN: usize = 907;
/// `disable_flags` bits (IDL docs): 3 = buy disabled, 4 = sell disabled.
const DISABLE_BUY: u8 = 1 << 3;
const DISABLE_SELL: u8 = 1 << 4;

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

/// The fields of `GlobalConfig` a swap needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalConfig {
    pub disable_flags: u8,
    pub protocol_fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
}

impl GlobalConfig {
    pub fn buy_disabled(&self) -> bool {
        self.disable_flags & DISABLE_BUY != 0
    }
    pub fn sell_disabled(&self) -> bool {
        self.disable_flags & DISABLE_SELL != 0
    }
}

fn first_recipient(d: &[u8], off: usize, n: usize) -> Option<Pubkey> {
    (0..n)
        .map(|i| Pubkey::new_from_array(d[off + 32 * i..off + 32 * (i + 1)].try_into().unwrap()))
        .find(|k| *k != Pubkey::default())
}

/// Decode `GlobalConfig` owned by `owner` (pure, fail-closed: wrong owner,
/// discriminator, a layout without buyback recipients, or no recipient set).
pub fn parse_global_config(owner: &Pubkey, data: &[u8]) -> Result<GlobalConfig, String> {
    if *owner != pk(PUMP_AMM_PROGRAM) {
        return Err(format!("GlobalConfig is owned by {owner}, not PumpSwap"));
    }
    if data.len() < GC_MIN_LEN {
        return Err(format!(
            "GlobalConfig is {} bytes; expected at least {GC_MIN_LEN}",
            data.len()
        ));
    }
    if data[..8] != GLOBAL_CONFIG_DISCRIMINATOR {
        return Err("account is not a PumpSwap GlobalConfig (discriminator)".into());
    }
    Ok(GlobalConfig {
        disable_flags: data[GC_DISABLE_FLAGS],
        protocol_fee_recipient: first_recipient(data, GC_PROTOCOL_RECIPIENTS, 8)
            .ok_or("GlobalConfig has no protocol fee recipient")?,
        buyback_fee_recipient: first_recipient(data, GC_BUYBACK_RECIPIENTS, 8)
            .ok_or("GlobalConfig has no buyback fee recipient")?,
    })
}

fn amm_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &pk(PUMP_AMM_PROGRAM)).0
}

pub fn global_config_address() -> Pubkey {
    amm_pda(&[b"global_config"])
}
pub fn event_authority() -> Pubkey {
    amm_pda(&[b"__event_authority"])
}
pub fn global_volume_accumulator() -> Pubkey {
    amm_pda(&[b"global_volume_accumulator"])
}
pub fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
    amm_pda(&[b"user_volume_accumulator", user.as_ref()])
}
pub fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    amm_pda(&[b"creator_vault", coin_creator.as_ref()])
}
pub fn pool_v2(base_mint: &Pubkey) -> Pubkey {
    amm_pda(&[b"pool-v2", base_mint.as_ref()])
}

/// Associated token account of `owner` for `mint` under `token_program`.
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pk(ASSOCIATED_TOKEN_PROGRAM),
    )
    .0
}

/// Both token programs a PumpSwap base mint may belong to.
pub fn check_token_program(p: &Pubkey) -> Result<(), String> {
    if *p == pk(SPL_TOKEN_PROGRAM) || *p == pk(TOKEN_2022_PROGRAM) {
        Ok(())
    } else {
        Err(format!("{p} is not a token program"))
    }
}

/// Swap direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `buy(base_amount_out, max_quote_amount_in)`: wSOL in, base out.
    Buy { base_out: u64, max_quote_in: u64 },
    /// `sell(base_amount_in, min_quote_amount_out)`: base in, wSOL out.
    Sell { base_in: u64, min_quote_out: u64 },
}

/// The PumpSwap `buy` / `sell` instruction itself (IDL account order plus
/// the SDK's remaining accounts).
pub fn swap_ix(
    pool_id: &Pubkey,
    pool: &PumpPool,
    gc: &GlobalConfig,
    user: &Pubkey,
    base_token_program: &Pubkey,
    side: Side,
) -> Result<Instruction, String> {
    check_token_program(base_token_program)?;
    let wsol = pk(WSOL_MINT);
    if pool.quote_mint != wsol {
        return Err("only wSOL-quoted PumpSwap pools are supported".into());
    }
    match side {
        Side::Buy { .. } if gc.buy_disabled() => return Err("PumpSwap buys are disabled".into()),
        Side::Sell { .. } if gc.sell_disabled() => return Err("PumpSwap sells are disabled".into()),
        _ => {}
    }
    let quote_program = pk(SPL_TOKEN_PROGRAM);
    let creator_vault = coin_creator_vault_authority(&pool.coin_creator);
    let fee_program = pk(PUMP_FEE_PROGRAM);

    let mut accounts = vec![
        AccountMeta::new(*pool_id, false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(global_config_address(), false),
        AccountMeta::new_readonly(pool.base_mint, false),
        AccountMeta::new_readonly(pool.quote_mint, false),
        AccountMeta::new(ata(user, &pool.base_mint, base_token_program), false),
        AccountMeta::new(ata(user, &wsol, &quote_program), false),
        AccountMeta::new(pool.pool_base_token_account, false),
        AccountMeta::new(pool.pool_quote_token_account, false),
        AccountMeta::new_readonly(gc.protocol_fee_recipient, false),
        AccountMeta::new(
            ata(&gc.protocol_fee_recipient, &wsol, &quote_program),
            false,
        ),
        AccountMeta::new_readonly(*base_token_program, false),
        AccountMeta::new_readonly(quote_program, false),
        AccountMeta::new_readonly(pk(SYSTEM_PROGRAM), false),
        AccountMeta::new_readonly(pk(ASSOCIATED_TOKEN_PROGRAM), false),
        AccountMeta::new_readonly(event_authority(), false),
        AccountMeta::new_readonly(pk(PUMP_AMM_PROGRAM), false),
        AccountMeta::new(ata(&creator_vault, &wsol, &quote_program), false),
        AccountMeta::new_readonly(creator_vault, false),
    ];
    let mut data = Vec::with_capacity(25);
    match side {
        Side::Buy {
            base_out,
            max_quote_in,
        } => {
            accounts.push(AccountMeta::new_readonly(
                global_volume_accumulator(),
                false,
            ));
            accounts.push(AccountMeta::new(user_volume_accumulator(user), false));
            data.extend_from_slice(&BUY_DISCRIMINATOR);
            data.extend_from_slice(&base_out.to_le_bytes());
            data.extend_from_slice(&max_quote_in.to_le_bytes());
            data.push(1); // track_volume: OptionBool(true), as the SDK sends
        }
        Side::Sell {
            base_in,
            min_quote_out,
        } => {
            data.extend_from_slice(&SELL_DISCRIMINATOR);
            data.extend_from_slice(&base_in.to_le_bytes());
            data.extend_from_slice(&min_quote_out.to_le_bytes());
        }
    }
    accounts.push(AccountMeta::new_readonly(
        super::pumpswap::fee_config_address(),
        false,
    ));
    accounts.push(AccountMeta::new_readonly(fee_program, false));

    // Remaining accounts, in the SDK's order.
    if pool.is_cashback_coin {
        let uva = user_volume_accumulator(user);
        accounts.push(AccountMeta::new(ata(&uva, &wsol, &quote_program), false));
        if matches!(side, Side::Sell { .. }) {
            accounts.push(AccountMeta::new(uva, false));
        }
    }
    if pool.coin_creator != Pubkey::default() {
        accounts.push(AccountMeta::new_readonly(pool_v2(&pool.base_mint), false));
    }
    accounts.push(AccountMeta::new_readonly(gc.buyback_fee_recipient, false));
    accounts.push(AccountMeta::new(
        ata(&gc.buyback_fee_recipient, &wsol, &quote_program),
        false,
    ));
    Ok(Instruction {
        program_id: pk(PUMP_AMM_PROGRAM),
        accounts,
        data,
    })
}

/// `extend_account` for a pool account shorter than
/// [`POOL_ACCOUNT_NEW_SIZE`] (the SDK prepends it to every trade then).
pub fn extend_pool_ix(pool_id: &Pubkey, user: &Pubkey) -> Instruction {
    Instruction {
        program_id: pk(PUMP_AMM_PROGRAM),
        accounts: vec![
            AccountMeta::new(*pool_id, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(pk(SYSTEM_PROGRAM), false),
            AccountMeta::new_readonly(event_authority(), false),
            AccountMeta::new_readonly(pk(PUMP_AMM_PROGRAM), false),
        ],
        data: EXTEND_ACCOUNT_DISCRIMINATOR.to_vec(),
    }
}

/// ATA program `CreateIdempotent` for `owner`'s ATA of `mint`.
pub fn create_ata_idempotent_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: pk(ASSOCIATED_TOKEN_PROGRAM),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(pk(SYSTEM_PROGRAM), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// The full instruction list of a simulated swap, the SDK's user-side
/// setup: optional pool extension; for a buy, wrap `max_quote_in` lamports
/// into the user's wSOL ATA and create the base ATA; for a sell, create the
/// wSOL ATA. The output is never unwrapped (simulation reads it).
pub fn swap_instructions(
    pool_id: &Pubkey,
    pool_data_len: usize,
    pool: &PumpPool,
    gc: &GlobalConfig,
    user: &Pubkey,
    base_token_program: &Pubkey,
    side: Side,
) -> Result<Vec<Instruction>, String> {
    let wsol = pk(WSOL_MINT);
    let spl = pk(SPL_TOKEN_PROGRAM);
    let user_wsol = ata(user, &wsol, &spl);
    let mut ixs = Vec::new();
    if pool_data_len < POOL_ACCOUNT_NEW_SIZE {
        ixs.push(extend_pool_ix(pool_id, user));
    }
    ixs.push(create_ata_idempotent_ix(user, user, &wsol, &spl));
    if let Side::Buy { max_quote_in, .. } = side {
        ixs.push(solana_system_interface::instruction::transfer(
            user,
            &user_wsol,
            max_quote_in,
        ));
        ixs.push(Instruction {
            program_id: spl,
            accounts: vec![AccountMeta::new(user_wsol, false)],
            data: vec![17], // SyncNative
        });
        ixs.push(create_ata_idempotent_ix(
            user,
            user,
            &pool.base_mint,
            base_token_program,
        ));
    }
    ixs.push(swap_ix(pool_id, pool, gc, user, base_token_program, side)?);
    Ok(ixs)
}

/// A wallet that can stand in as the seller of an exit SIMULATION: an
/// on-curve owner whose ATA (under `token_program`) holds at least
/// `min_amount` of `mint`. `accounts` are `(token account, data)`. Pure.
pub fn pick_holder(
    mint: &Pubkey,
    token_program: &Pubkey,
    accounts: &[(Pubkey, Option<Vec<u8>>)],
    min_amount: u64,
) -> Option<Pubkey> {
    accounts.iter().find_map(|(addr, data)| {
        let d = data.as_ref().filter(|d| d.len() >= 72)?;
        let owner = Pubkey::new_from_array(d[32..64].try_into().ok()?);
        let amount = u64::from_le_bytes(d[64..72].try_into().ok()?);
        (owner.is_on_curve() && amount >= min_amount && *addr == ata(&owner, mint, token_program))
            .then_some(owner)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_disc(name: &str) -> [u8; 8] {
        solana_sdk::hash::hash(format!("global:{name}").as_bytes()).to_bytes()[..8]
            .try_into()
            .unwrap()
    }

    fn pool() -> PumpPool {
        PumpPool {
            creator: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: pk(WSOL_MINT),
            pool_base_token_account: Pubkey::new_unique(),
            pool_quote_token_account: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            virtual_quote_reserves: 0,
            creator_fee_bps: 0,
            protocol_fees: 0,
            creator_fees: 0,
        }
    }

    fn gc() -> GlobalConfig {
        GlobalConfig {
            disable_flags: 0,
            protocol_fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: Pubkey::new_unique(),
        }
    }

    #[test]
    fn discriminators_match_anchor() {
        assert_eq!(BUY_DISCRIMINATOR, anchor_disc("buy"));
        assert_eq!(SELL_DISCRIMINATOR, anchor_disc("sell"));
        assert_eq!(EXTEND_ACCOUNT_DISCRIMINATOR, anchor_disc("extend_account"));
        let acc: [u8; 8] = solana_sdk::hash::hash(b"account:GlobalConfig").to_bytes()[..8]
            .try_into()
            .unwrap();
        assert_eq!(GLOBAL_CONFIG_DISCRIMINATOR, acc);
        let pool_acc: [u8; 8] = solana_sdk::hash::hash(b"account:Pool").to_bytes()[..8]
            .try_into()
            .unwrap();
        assert_eq!(super::super::pumpswap::POOL_DISCRIMINATOR, pool_acc);
    }

    #[test]
    fn buy_and_sell_follow_the_idl_and_sdk_account_order() {
        let (pool_id, user) = (Pubkey::new_unique(), Pubkey::new_unique());
        let p = pool();
        let g = gc();
        let spl = pk(SPL_TOKEN_PROGRAM);
        let buy = swap_ix(
            &pool_id,
            &p,
            &g,
            &user,
            &spl,
            Side::Buy {
                base_out: 7,
                max_quote_in: 9,
            },
        )
        .unwrap();
        // 23 IDL accounts + pool-v2 + buyback recipient + its ATA.
        assert_eq!(buy.accounts.len(), 26);
        assert_eq!(buy.accounts[0].pubkey, pool_id);
        assert!(buy.accounts[1].is_signer && buy.accounts[1].pubkey == user);
        assert_eq!(buy.accounts[2].pubkey, global_config_address());
        assert_eq!(buy.accounts[5].pubkey, ata(&user, &p.base_mint, &spl));
        assert_eq!(buy.accounts[9].pubkey, g.protocol_fee_recipient);
        assert_eq!(buy.accounts[16].pubkey, pk(PUMP_AMM_PROGRAM));
        assert_eq!(buy.accounts[19].pubkey, global_volume_accumulator());
        assert_eq!(buy.accounts[20].pubkey, user_volume_accumulator(&user));
        assert_eq!(
            buy.accounts[21].pubkey,
            super::super::pumpswap::fee_config_address()
        );
        assert_eq!(buy.accounts[22].pubkey, pk(PUMP_FEE_PROGRAM));
        assert_eq!(buy.accounts[23].pubkey, pool_v2(&p.base_mint));
        assert_eq!(buy.accounts[24].pubkey, g.buyback_fee_recipient);
        assert!(buy.accounts[25].is_writable);
        assert_eq!(&buy.data[..8], &BUY_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(buy.data[8..16].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(buy.data[16..24].try_into().unwrap()), 9);
        assert_eq!(buy.data.len(), 25);

        let sell = swap_ix(
            &pool_id,
            &p,
            &g,
            &user,
            &spl,
            Side::Sell {
                base_in: 5,
                min_quote_out: 1,
            },
        )
        .unwrap();
        // 21 IDL accounts (no volume accumulators) + 3 remaining.
        assert_eq!(sell.accounts.len(), 24);
        assert_eq!(
            sell.accounts[19].pubkey,
            super::super::pumpswap::fee_config_address()
        );
        assert_eq!(&sell.data[..8], &SELL_DISCRIMINATOR);
        assert_eq!(sell.data.len(), 24);

        // Cashback coin: user volume accumulator ATA (and, on sell, the
        // accumulator itself) come first among the remaining accounts.
        let mut cb = p.clone();
        cb.is_cashback_coin = true;
        let s = swap_ix(
            &pool_id,
            &cb,
            &g,
            &user,
            &spl,
            Side::Sell {
                base_in: 5,
                min_quote_out: 1,
            },
        )
        .unwrap();
        assert_eq!(s.accounts.len(), 26);
        assert_eq!(s.accounts[22].pubkey, user_volume_accumulator(&user));
        // No coin creator: no pool-v2 account.
        let mut nc = p.clone();
        nc.coin_creator = Pubkey::default();
        let b = swap_ix(
            &pool_id,
            &nc,
            &g,
            &user,
            &spl,
            Side::Buy {
                base_out: 1,
                max_quote_in: 1,
            },
        )
        .unwrap();
        assert_eq!(b.accounts.len(), 25);
    }

    #[test]
    fn swaps_fail_closed() {
        let (pool_id, user) = (Pubkey::new_unique(), Pubkey::new_unique());
        let spl = pk(SPL_TOKEN_PROGRAM);
        let buy = Side::Buy {
            base_out: 1,
            max_quote_in: 1,
        };
        let mut off = gc();
        off.disable_flags = DISABLE_BUY;
        assert!(swap_ix(&pool_id, &pool(), &off, &user, &spl, buy).is_err());
        assert!(swap_ix(
            &pool_id,
            &pool(),
            &off,
            &user,
            &spl,
            Side::Sell {
                base_in: 1,
                min_quote_out: 1
            }
        )
        .is_ok());
        let mut usdc = pool();
        usdc.quote_mint = Pubkey::new_unique();
        assert!(swap_ix(&pool_id, &usdc, &gc(), &user, &spl, buy).is_err());
        assert!(swap_ix(&pool_id, &pool(), &gc(), &user, &Pubkey::new_unique(), buy).is_err());
    }

    #[test]
    fn global_config_layout() {
        let amm = pk(PUMP_AMM_PROGRAM);
        let (fee, buyback) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut d = vec![0u8; 949];
        d[..8].copy_from_slice(&GLOBAL_CONFIG_DISCRIMINATOR);
        d[GC_DISABLE_FLAGS] = DISABLE_SELL;
        // First recipient slot empty, the second set: the first non-default wins.
        d[GC_PROTOCOL_RECIPIENTS + 32..GC_PROTOCOL_RECIPIENTS + 64].copy_from_slice(fee.as_ref());
        d[GC_BUYBACK_RECIPIENTS..GC_BUYBACK_RECIPIENTS + 32].copy_from_slice(buyback.as_ref());
        let g = parse_global_config(&amm, &d).unwrap();
        assert_eq!(
            (g.protocol_fee_recipient, g.buyback_fee_recipient),
            (fee, buyback)
        );
        assert!(g.sell_disabled() && !g.buy_disabled());
        assert!(parse_global_config(&Pubkey::new_unique(), &d).is_err());
        assert!(parse_global_config(&amm, &d[..GC_MIN_LEN - 1]).is_err());
        let mut no_buyback = d.clone();
        no_buyback[GC_BUYBACK_RECIPIENTS..GC_BUYBACK_RECIPIENTS + 32].fill(0);
        assert!(parse_global_config(&amm, &no_buyback).is_err());
        let mut disc = d.clone();
        disc[0] ^= 1;
        assert!(parse_global_config(&amm, &disc).is_err());
    }

    #[test]
    fn holder_must_be_an_on_curve_ata_with_enough_tokens() {
        use solana_sdk::signature::Signer;
        let mint = Pubkey::new_unique();
        let t22 = pk(TOKEN_2022_PROGRAM);
        let wallet = solana_sdk::signature::Keypair::new().pubkey();
        let (pda, _) = Pubkey::find_program_address(&[b"x"], &Pubkey::new_unique());
        let acct = |owner: &Pubkey, amount: u64| {
            let mut d = vec![0u8; 165];
            d[32..64].copy_from_slice(owner.as_ref());
            d[64..72].copy_from_slice(&amount.to_le_bytes());
            Some(d)
        };
        let list = vec![
            (ata(&pda, &mint, &t22), acct(&pda, 1_000)), // program-owned
            (Pubkey::new_unique(), acct(&wallet, 1_000)), // not the ATA
            (
                ata(&wallet, &mint, &pk(SPL_TOKEN_PROGRAM)),
                acct(&wallet, 1_000),
            ), // wrong program
            (ata(&wallet, &mint, &t22), acct(&wallet, 10)), // too small
        ];
        assert_eq!(pick_holder(&mint, &t22, &list, 100), None);
        let mut ok = list.clone();
        ok.push((ata(&wallet, &mint, &t22), acct(&wallet, 500)));
        assert_eq!(pick_holder(&mint, &t22, &ok, 100), Some(wallet));
    }

    #[test]
    fn buy_setup_wraps_sol_and_extends_short_pools() {
        let (pool_id, user) = (Pubkey::new_unique(), Pubkey::new_unique());
        let spl = pk(SPL_TOKEN_PROGRAM);
        let t22 = pk(TOKEN_2022_PROGRAM);
        let p = pool();
        let ixs = swap_instructions(
            &pool_id,
            287,
            &p,
            &gc(),
            &user,
            &t22,
            Side::Buy {
                base_out: 1,
                max_quote_in: 10_000_000,
            },
        )
        .unwrap();
        // extend, wSOL ATA, transfer, sync, base ATA (Token-2022), buy.
        assert_eq!(ixs.len(), 6);
        assert_eq!(ixs[0].data, EXTEND_ACCOUNT_DISCRIMINATOR.to_vec());
        assert_eq!(ixs[4].accounts[1].pubkey, ata(&user, &p.base_mint, &t22));
        assert_eq!(ixs[4].accounts[5].pubkey, t22);
        assert_eq!(ixs[5].accounts[5].pubkey, ata(&user, &p.base_mint, &t22));
        let sell = swap_instructions(
            &pool_id,
            300,
            &p,
            &gc(),
            &user,
            &spl,
            Side::Sell {
                base_in: 1,
                min_quote_out: 1,
            },
        )
        .unwrap();
        // No extension at the new size; wSOL ATA + sell, output not unwrapped.
        assert_eq!(sell.len(), 2);
    }
}
