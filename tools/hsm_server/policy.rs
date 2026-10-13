//! Signing policy: the HSM signs only transactions shaped like the bot's own
//! swaps and keeps every fund movement inside the signer's wallet.
//!
//! A client that holds the mTLS identity can otherwise get ANY transaction
//! signed (e.g. a transfer of the whole balance to an attacker). Allowed:
//! - ComputeBudget (unit price capped),
//! - ATA program create / create-idempotent for an ATA of the signer,
//! - System transfer from the signer to itself, to one of its ATAs created in
//!   the same transaction (wSOL wrap), or to a Jito tip / operator-allowed
//!   account — the total leaving the wallet is capped,
//! - SPL Token SyncNative on a signer ATA, CloseAccount of a signer ATA back
//!   to the signer,
//! - Raydium CLMM `swap` paid by the signer between two signer ATAs.
//!
//! Everything else (token transfers / approvals / authority changes, other
//! programs, other CLMM instructions, extra signers) is rejected.

use std::collections::HashSet;
use std::str::FromStr;

use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;

const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const CLMM_PROGRAMS: [&str; 2] = [
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // mainnet
    "DRayAUgENGQBKVaX8owNhgzkEDyoHTGVEGHVJT1E9pfH", // devnet
];
/// sha256("global:swap")[..8] — the only CLMM instruction the bot sends.
const CLMM_SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
/// Jito Block Engine mainnet tip accounts (getTipAccounts).
const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Limits; defaults fit the bot's hard risk caps (0.05 SOL/trade).
#[derive(Debug, Clone)]
pub struct Policy {
    /// Max lamports a transaction may move out of the signer's own account
    /// (wSOL wrap + tip). Default 0.06 SOL.
    pub max_transfer_lamports: u64,
    /// Max compute-unit price (micro-lamports). Default 5_000_000.
    pub max_cu_price_micro_lamports: u64,
    /// Extra transfer destinations (besides the Jito tip accounts).
    pub extra_destinations: HashSet<Pubkey>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_transfer_lamports: 60_000_000,
            max_cu_price_micro_lamports: 5_000_000,
            extra_destinations: HashSet::new(),
        }
    }
}

impl Policy {
    /// `HSM_MAX_TRANSFER_LAMPORTS`, `HSM_MAX_CU_PRICE_MICRO_LAMPORTS`,
    /// `HSM_EXTRA_TRANSFER_DESTINATIONS` (comma-separated pubkeys).
    pub fn from_env() -> Result<Self, String> {
        let mut p = Policy::default();
        if let Ok(v) = std::env::var("HSM_MAX_TRANSFER_LAMPORTS") {
            if !v.trim().is_empty() {
                p.max_transfer_lamports = v
                    .trim()
                    .parse()
                    .map_err(|e| format!("HSM_MAX_TRANSFER_LAMPORTS: {e}"))?;
            }
        }
        if let Ok(v) = std::env::var("HSM_MAX_CU_PRICE_MICRO_LAMPORTS") {
            if !v.trim().is_empty() {
                p.max_cu_price_micro_lamports = v
                    .trim()
                    .parse()
                    .map_err(|e| format!("HSM_MAX_CU_PRICE_MICRO_LAMPORTS: {e}"))?;
            }
        }
        if let Ok(v) = std::env::var("HSM_EXTRA_TRANSFER_DESTINATIONS") {
            for s in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                p.extra_destinations.insert(
                    Pubkey::from_str(s)
                        .map_err(|e| format!("HSM_EXTRA_TRANSFER_DESTINATIONS {s}: {e}"))?,
                );
            }
        }
        Ok(p)
    }
}

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("valid constant pubkey")
}

fn ata_of(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pk(ATA_PROGRAM),
    )
    .0
}

/// Check `message` (to be signed by `signer`) against the policy.
pub fn check(message: &Message, signer: &Pubkey, policy: &Policy) -> Result<(), String> {
    let keys = &message.account_keys;
    if keys.first() != Some(signer) {
        return Err("fee payer is not the HSM signer".into());
    }
    if message.header.num_required_signatures != 1 {
        return Err(format!(
            "transaction requires {} signatures (only the HSM signer is allowed)",
            message.header.num_required_signatures
        ));
    }
    let key = |i: u8| -> Result<Pubkey, String> {
        keys.get(i as usize)
            .copied()
            .ok_or_else(|| "account index out of range".to_string())
    };

    let system = pk(SYSTEM_PROGRAM);
    let compute = pk(COMPUTE_BUDGET_PROGRAM);
    let token = pk(TOKEN_PROGRAM);
    let ata_program = pk(ATA_PROGRAM);
    let clmm: Vec<Pubkey> = CLMM_PROGRAMS.iter().map(|s| pk(s)).collect();
    let tips: HashSet<Pubkey> = JITO_TIP_ACCOUNTS.iter().map(|s| pk(s)).collect();

    // Pass 1: the signer's ATAs this transaction creates.
    let mut own_atas: HashSet<Pubkey> = HashSet::new();
    for ix in &message.instructions {
        if key(ix.program_id_index)? != ata_program {
            continue;
        }
        if !(ix.data.is_empty() || ix.data == [0] || ix.data == [1]) {
            return Err("ATA program: only create / create-idempotent allowed".into());
        }
        let a: Vec<Pubkey> = ix
            .accounts
            .iter()
            .map(|&i| key(i))
            .collect::<Result<_, _>>()?;
        if a.len() < 6 {
            return Err("ATA create: missing accounts".into());
        }
        let (payer, ata, owner, mint, token_program) = (a[0], a[1], a[2], a[3], a[5]);
        if payer != *signer || owner != *signer {
            return Err("ATA create: payer and owner must be the HSM signer".into());
        }
        if token_program != token {
            return Err("ATA create: only the classic SPL Token program".into());
        }
        if ata != ata_of(signer, &mint, &token_program) {
            return Err("ATA create: account is not the signer's associated token account".into());
        }
        own_atas.insert(ata);
    }

    // Pass 2: every instruction.
    let mut moved_out: u64 = 0;
    for ix in &message.instructions {
        let program = key(ix.program_id_index)?;
        let acc = |n: usize| -> Result<Pubkey, String> {
            ix.accounts
                .get(n)
                .ok_or_else(|| "missing instruction account".to_string())
                .and_then(|&i| key(i))
        };
        if program == compute {
            // 2 = SetComputeUnitLimit(u32), 3 = SetComputeUnitPrice(u64).
            match ix.data.first() {
                Some(3) => {
                    let price = ix
                        .data
                        .get(1..9)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_le_bytes)
                        .ok_or("ComputeBudget: malformed unit price")?;
                    if price > policy.max_cu_price_micro_lamports {
                        return Err(format!(
                            "compute unit price {price} > cap {}",
                            policy.max_cu_price_micro_lamports
                        ));
                    }
                }
                Some(1) | Some(2) | Some(4) => {}
                _ => return Err("ComputeBudget: unknown instruction".into()),
            }
        } else if program == ata_program {
            // validated in pass 1
        } else if program == system {
            // Transfer = u32 LE 2 + u64 lamports.
            if ix.data.len() != 12 || ix.data[..4] != [2, 0, 0, 0] {
                return Err("System program: only transfer allowed".into());
            }
            let lamports = u64::from_le_bytes(ix.data[4..12].try_into().unwrap());
            let (from, to) = (acc(0)?, acc(1)?);
            if from != *signer {
                return Err("System transfer: source is not the HSM signer".into());
            }
            if to == *signer {
                continue;
            }
            if !(own_atas.contains(&to)
                || tips.contains(&to)
                || policy.extra_destinations.contains(&to))
            {
                return Err(format!("System transfer to unapproved account {to}"));
            }
            moved_out = moved_out.saturating_add(lamports);
        } else if program == token {
            match ix.data.first() {
                // SyncNative(account)
                Some(17) if ix.data.len() == 1 => {
                    if !own_atas.contains(&acc(0)?) {
                        return Err("SyncNative on an account not created for the signer".into());
                    }
                }
                // CloseAccount(account, destination, authority)
                Some(9) if ix.data.len() == 1 => {
                    if !own_atas.contains(&acc(0)?) || acc(1)? != *signer || acc(2)? != *signer {
                        return Err(
                            "CloseAccount must close a signer ATA back to the signer".into()
                        );
                    }
                }
                _ => {
                    return Err(
                        "SPL Token: only SyncNative / CloseAccount allowed (no transfer/approve)"
                            .into(),
                    )
                }
            }
        } else if clmm.contains(&program) {
            if ix.data.get(..8) != Some(&CLMM_SWAP_DISCRIMINATOR[..]) {
                return Err("CLMM: only the swap instruction is allowed".into());
            }
            // payer, amm_config, pool_state, input_token_account, output_token_account, ...
            if acc(0)? != *signer {
                return Err("CLMM swap: payer is not the HSM signer".into());
            }
            if !own_atas.contains(&acc(3)?) || !own_atas.contains(&acc(4)?) {
                return Err("CLMM swap: input and output must be signer ATAs".into());
            }
        } else {
            return Err(format!("program {program} is not allowed"));
        }
    }
    if moved_out > policy.max_transfer_lamports {
        return Err(format!(
            "transfers out of the wallet {moved_out} > cap {} lamports",
            policy.max_transfer_lamports
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::{AccountMeta, Instruction};

    fn signer() -> Pubkey {
        Pubkey::new_unique()
    }

    fn create_ata(payer: &Pubkey, mint: &Pubkey) -> (Instruction, Pubkey) {
        let token = pk(TOKEN_PROGRAM);
        let ata = ata_of(payer, mint, &token);
        let ix = Instruction {
            program_id: pk(ATA_PROGRAM),
            accounts: vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(ata, false),
                AccountMeta::new_readonly(*payer, false),
                AccountMeta::new_readonly(*mint, false),
                AccountMeta::new_readonly(pk(SYSTEM_PROGRAM), false),
                AccountMeta::new_readonly(token, false),
            ],
            data: vec![1],
        };
        (ix, ata)
    }

    fn transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
        let mut data = vec![2, 0, 0, 0];
        data.extend_from_slice(&lamports.to_le_bytes());
        Instruction {
            program_id: pk(SYSTEM_PROGRAM),
            accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
            data,
        }
    }

    fn token_ix(tag: u8, accounts: Vec<AccountMeta>) -> Instruction {
        Instruction {
            program_id: pk(TOKEN_PROGRAM),
            accounts,
            data: vec![tag],
        }
    }

    fn swap(payer: &Pubkey, input: &Pubkey, output: &Pubkey) -> Instruction {
        let mut data = CLMM_SWAP_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[0u8; 33]);
        let mut accounts = vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(*input, false),
            AccountMeta::new(*output, false),
        ];
        accounts.extend((0..5).map(|_| AccountMeta::new(Pubkey::new_unique(), false)));
        Instruction {
            program_id: pk(CLMM_PROGRAMS[0]),
            accounts,
            data,
        }
    }

    fn cu_price(micro: u64) -> Instruction {
        let mut data = vec![3];
        data.extend_from_slice(&micro.to_le_bytes());
        Instruction {
            program_id: pk(COMPUTE_BUDGET_PROGRAM),
            accounts: vec![],
            data,
        }
    }

    /// The bot's entry: wrap SOL, swap wSOL -> token, close wSOL, tip.
    fn entry_tx(s: &Pubkey, wrap: u64) -> Vec<Instruction> {
        let wsol = pk("So11111111111111111111111111111111111111112");
        let (c_in, ata_in) = create_ata(s, &wsol);
        let (c_out, ata_out) = create_ata(s, &Pubkey::new_unique());
        vec![
            cu_price(10_000),
            c_in,
            c_out,
            transfer(s, &ata_in, wrap),
            token_ix(17, vec![AccountMeta::new(ata_in, false)]),
            swap(s, &ata_in, &ata_out),
            token_ix(
                9,
                vec![
                    AccountMeta::new(ata_in, false),
                    AccountMeta::new(*s, false),
                    AccountMeta::new_readonly(*s, true),
                ],
            ),
            transfer(s, &pk(JITO_TIP_ACCOUNTS[0]), 10_000),
        ]
    }

    fn check_ixs(ixs: &[Instruction], s: &Pubkey) -> Result<(), String> {
        check(&Message::new(ixs, Some(s)), s, &Policy::default())
    }

    #[test]
    fn bot_entry_and_self_transfer_are_allowed() {
        let s = signer();
        check_ixs(&entry_tx(&s, 10_000_000), &s).unwrap();
        check_ixs(&[transfer(&s, &s, 1_000)], &s).unwrap();
    }

    #[test]
    fn transfer_to_foreign_account_is_rejected() {
        let s = signer();
        let err = check_ixs(&[transfer(&s, &Pubkey::new_unique(), 1)], &s).unwrap_err();
        assert!(err.contains("unapproved"), "{err}");
        // Operator-approved destination passes.
        let dest = Pubkey::new_unique();
        let mut p = Policy::default();
        p.extra_destinations.insert(dest);
        check(&Message::new(&[transfer(&s, &dest, 1)], Some(&s)), &s, &p).unwrap();
    }

    #[test]
    fn wrap_above_cap_is_rejected() {
        let s = signer();
        let err = check_ixs(&entry_tx(&s, 2_000_000_000), &s).unwrap_err();
        assert!(err.contains("cap"), "{err}");
    }

    #[test]
    fn swap_into_foreign_token_account_is_rejected() {
        let s = signer();
        let wsol = pk("So11111111111111111111111111111111111111112");
        let (c_in, ata_in) = create_ata(&s, &wsol);
        let err = check_ixs(&[c_in, swap(&s, &ata_in, &Pubkey::new_unique())], &s).unwrap_err();
        assert!(err.contains("signer ATAs"), "{err}");
    }

    #[test]
    fn token_transfer_and_unknown_programs_are_rejected() {
        let s = signer();
        let (c, ata) = create_ata(&s, &Pubkey::new_unique());
        // SPL Token Transfer (3)
        let t = token_ix(
            3,
            vec![
                AccountMeta::new(ata, false),
                AccountMeta::new(Pubkey::new_unique(), false),
                AccountMeta::new_readonly(s, true),
            ],
        );
        assert!(check_ixs(&[c.clone(), t], &s).is_err());
        // CloseAccount to a foreign destination
        let close = token_ix(
            9,
            vec![
                AccountMeta::new(ata, false),
                AccountMeta::new(Pubkey::new_unique(), false),
                AccountMeta::new_readonly(s, true),
            ],
        );
        assert!(check_ixs(&[c, close], &s).is_err());
        let other = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new(s, true)],
            data: vec![],
        };
        assert!(check_ixs(&[other], &s).is_err());
    }

    #[test]
    fn ata_for_foreign_owner_and_fee_payer_are_rejected() {
        let s = signer();
        let other = signer();
        // ATA owned by someone else, paid by the signer.
        let (mut ix, _) = create_ata(&other, &Pubkey::new_unique());
        ix.accounts[0] = AccountMeta::new(s, true);
        assert!(check_ixs(&[ix], &s).is_err());
        // Fee payer is not the HSM signer.
        let msg = Message::new(&[transfer(&other, &other, 1)], Some(&other));
        assert!(check(&msg, &s, &Policy::default()).is_err());
    }

    #[test]
    fn compute_price_cap_and_policy_env_parsing() {
        let s = signer();
        assert!(check_ixs(&[cu_price(5_000_000)], &s).is_ok());
        assert!(check_ixs(&[cu_price(5_000_001)], &s).is_err());
        let p = Policy::default();
        assert_eq!(p.max_transfer_lamports, 60_000_000);
    }
}
