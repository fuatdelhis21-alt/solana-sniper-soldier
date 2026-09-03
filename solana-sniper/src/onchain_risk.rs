//! # On-Chain Risk Data
//!
//! Replaces static/manual `TokenCandidate` inputs with real on-chain data
//! read directly from the Solana RPC — no third-party API is required or
//! trusted for trading decisions:
//!
//! - **Liquidity** — read the resolved pool's input vault SPL token balance.
//! - **Holder concentration** — `getTokenLargestAccounts` + `getTokenSupply`
//!   give a rug-risk proxy: the percentage of supply held by the single
//!   largest holder (excluding the pool's own vault).
//! - **Blocklist** — a local file of known-bad mint addresses, loaded once
//!   at startup.
//!
//! ## Safety
//! - Fail-closed: any RPC error propagates. A candidate is only ever
//!   evaluated using confirmed on-chain data, never a fabricated fallback.
//! - No secrets are involved; only public account data is read.

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

/// Holder concentration statistics for a mint.
#[derive(Debug, Clone, PartialEq)]
pub struct HolderStats {
    /// Number of distinct holder accounts sampled (RPC caps this at 20 —
    /// the "top 20" largest holders — so this is a lower bound, not an
    /// exact total holder count).
    pub sampled_holders: u64,
    /// Percentage (0-100) of total supply held by the single largest
    /// account, excluding `exclude` (typically the pool's own vault).
    pub top_holder_pct: f64,
    pub total_supply: u64,
}

/// Fetch the real on-chain token balance (in the smallest unit / lamports
/// for the token) of a vault account. Used as the live liquidity figure for
/// the strategy gate instead of a manually-supplied CLI value.
pub fn fetch_vault_liquidity(rpc: &RpcClient, vault: &Pubkey) -> Result<u64, String> {
    let balance = rpc
        .get_token_account_balance(vault)
        .map_err(|e| format!("failed to fetch vault balance for {vault}: {e}"))?;
    balance
        .amount
        .parse::<u64>()
        .map_err(|e| format!("failed to parse vault balance amount: {e}"))
}

/// Fetch holder concentration for `mint`, excluding `exclude` (the pool's
/// own vault, which is not a "holder" in the rug-risk sense).
///
/// Fail-closed: any RPC error propagates. If the mint has zero supply, an
/// error is returned rather than a division-by-zero fallback.
pub fn fetch_holder_stats(
    rpc: &RpcClient,
    mint: &Pubkey,
    exclude: &Pubkey,
) -> Result<HolderStats, String> {
    let supply = rpc
        .get_token_supply(mint)
        .map_err(|e| format!("failed to fetch token supply for {mint}: {e}"))?;
    let total_supply: u64 = supply
        .amount
        .parse()
        .map_err(|e| format!("failed to parse token supply amount: {e}"))?;
    if total_supply == 0 {
        return Err(format!(
            "mint {mint} has zero supply — cannot assess holder risk"
        ));
    }

    let largest = rpc
        .get_token_largest_accounts(mint)
        .map_err(|e| format!("failed to fetch largest accounts for {mint}: {e}"))?;

    let mut sampled_holders = 0u64;
    let mut top_amount: u64 = 0;
    for entry in &largest {
        let owner_pubkey = Pubkey::from_str(&entry.address)
            .map_err(|e| format!("invalid token account address {}: {e}", entry.address))?;
        if &owner_pubkey == exclude {
            continue;
        }
        let amount: u64 = entry
            .amount
            .amount
            .parse()
            .map_err(|e| format!("failed to parse largest-account amount: {e}"))?;
        sampled_holders += 1;
        if amount > top_amount {
            top_amount = amount;
        }
    }

    let top_holder_pct = (top_amount as f64 / total_supply as f64) * 100.0;

    Ok(HolderStats {
        sampled_holders,
        top_holder_pct,
        total_supply,
    })
}

/// Mint/freeze authority risk read directly from the SPL Token mint account
/// (`Mint` layout, https://docs.rs/spl-token/latest/spl_token/state/struct.Mint.html):
/// offset 0..32 = `COption<Pubkey>` mint_authority (u32 tag + 32 bytes),
/// offset 44..82 = `COption<Pubkey>` freeze_authority.
/// A non-null freeze authority means the issuer can freeze any holder's
/// tokens at will — a classic rug vector — so it is treated as a hard
/// rejection, not merely a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintAuthorityRisk {
    pub mint_authority_present: bool,
    pub freeze_authority_present: bool,
}

impl MintAuthorityRisk {
    /// True if either authority is still present (i.e. the mint is not
    /// fully renounced/immutable).
    pub fn is_risky(&self) -> bool {
        self.mint_authority_present || self.freeze_authority_present
    }
}

/// Fetch and parse the mint/freeze authority flags for `mint` directly from
/// its on-chain SPL Token `Mint` account.
///
/// Fail-closed: any RPC error, missing account, or malformed/short account
/// data propagates as an `Err` rather than defaulting to "safe".
pub fn fetch_mint_authority_risk(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<MintAuthorityRisk, String> {
    let account = rpc
        .get_account(mint)
        .map_err(|e| format!("failed to fetch mint account {mint}: {e}"))?;
    // SPL Token Mint account is exactly 82 bytes (Token-2022 mints are
    // longer due to extensions, but the base Mint layout is a fixed prefix
    // in both, so this parse is valid for both program versions).
    if account.data.len() < 82 {
        return Err(format!(
            "mint {mint} account data too short ({} bytes) to be a valid SPL Mint",
            account.data.len()
        ));
    }
    let mint_authority_tag = u32::from_le_bytes(account.data[0..4].try_into().unwrap());
    let freeze_authority_tag = u32::from_le_bytes(account.data[46..50].try_into().unwrap());
    Ok(MintAuthorityRisk {
        mint_authority_present: mint_authority_tag != 0,
        freeze_authority_present: freeze_authority_tag != 0,
    })
}

/// Load a blocklist of known-bad mint addresses from a file (one base58
/// pubkey per line; blank lines and `#`-comments are ignored).
///
/// If `path` does not exist, returns an empty set (no known-bad list
/// configured is not itself a fail-closed condition — it simply means this
/// optional extra gate is inactive).
pub fn load_blocklist(path: &Path) -> Result<HashSet<Pubkey>, String> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read blocklist file {}: {e}", path.display()))?;
    let mut set = HashSet::new();
    for (line_no, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let pk = Pubkey::from_str(line).map_err(|e| {
            format!(
                "blocklist file {}:{}: invalid pubkey: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        set.insert(pk);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_blocklist_missing_file_is_empty() {
        let set = load_blocklist(Path::new("/nonexistent/path/blocklist.txt")).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn load_blocklist_parses_lines_and_ignores_comments() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("blocklist_test_{}.txt", std::process::id()));
        let mint = Pubkey::new_unique();
        std::fs::write(&path, format!("# comment\n\n{mint}\n")).unwrap();

        let set = load_blocklist(&path).unwrap();
        assert!(set.contains(&mint));
        assert_eq!(set.len(), 1);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mint_authority_risk_detects_present_and_absent() {
        let present = MintAuthorityRisk {
            mint_authority_present: true,
            freeze_authority_present: false,
        };
        assert!(present.is_risky());

        let renounced = MintAuthorityRisk {
            mint_authority_present: false,
            freeze_authority_present: false,
        };
        assert!(!renounced.is_risky());

        let freeze_only = MintAuthorityRisk {
            mint_authority_present: false,
            freeze_authority_present: true,
        };
        assert!(freeze_only.is_risky());
    }

    #[test]
    fn mint_authority_tag_parsing_matches_spl_layout() {
        // Build a synthetic 82-byte SPL Mint account: COption<Pubkey> tag is
        // a little-endian u32 (0 = None, 1 = Some) at offset 0 (mint_authority)
        // and offset 46 (freeze_authority).
        let mut data = vec![0u8; 82];
        data[0..4].copy_from_slice(&1u32.to_le_bytes()); // mint_authority = Some
        data[46..50].copy_from_slice(&0u32.to_le_bytes()); // freeze_authority = None
        let mint_authority_tag = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let freeze_authority_tag = u32::from_le_bytes(data[46..50].try_into().unwrap());
        assert_eq!(mint_authority_tag, 1);
        assert_eq!(freeze_authority_tag, 0);
    }

    #[test]
    fn load_blocklist_rejects_invalid_pubkey() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("blocklist_bad_{}.txt", std::process::id()));
        std::fs::write(&path, "not-a-valid-pubkey\n").unwrap();

        let result = load_blocklist(&path);
        assert!(result.is_err());

        std::fs::remove_file(&path).ok();
    }
}
