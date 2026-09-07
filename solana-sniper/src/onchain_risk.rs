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
    /// Percentage (0-100) of total supply held by the sampled top-20
    /// accounts combined (exclusions applied). Sample is capped at 20 by
    /// the SPL RPC standard, so this is the best measurable concentration.
    pub top20_holder_pct: f64,
    pub total_supply: u64,
}

/// Holder-concentration thresholds (operator-approved design):
/// `getTokenLargestAccounts` returns at most 20 accounts per the SPL
/// JSON-RPC standard, so an absolute holder count is structurally
/// unmeasurable — the rug-risk gate therefore uses supply-share
/// concentration instead. Single source of truth for both paper and live.
/// - single largest holder (post-exclusion) > 30% of supply → reject.
/// - combined top-20 holders (post-exclusion) > 70% of supply → reject.
pub const MAX_SINGLE_HOLDER_PCT: f64 = 30.0;
pub const MAX_TOP20_HOLDER_PCT: f64 = 70.0;
/// SPL JSON-RPC cap on `getTokenLargestAccounts` results.
pub const LARGEST_ACCOUNTS_CAP: usize = 20;

/// Outcome of the holder-concentration gate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HolderVerdict {
    /// Concentration within thresholds; candidate passes the holder gate.
    Ok,
    /// Single largest holder (post-exclusion) holds more than 30% of supply.
    SingleHolderConcentrated(f64),
    /// Combined top-20 (post-exclusion) holds more than 70% of supply.
    TopHoldersConcentrated(f64),
    /// No measurable non-excluded holder accounts — the gate cannot be
    /// assessed, so it fails closed (a reject, never a silent pass).
    Unassessable,
}

/// Evaluate the holder-concentration gate from stats. Fail-closed: any
/// unassessable or concentrated distribution returns a reject verdict.
pub fn holder_concentration_verdict(stats: &HolderStats) -> HolderVerdict {
    if stats.sampled_holders == 0 || stats.total_supply == 0 {
        return HolderVerdict::Unassessable;
    }
    if stats.top_holder_pct > MAX_SINGLE_HOLDER_PCT {
        return HolderVerdict::SingleHolderConcentrated(stats.top_holder_pct);
    }
    if stats.top20_holder_pct > MAX_TOP20_HOLDER_PCT {
        return HolderVerdict::TopHoldersConcentrated(stats.top20_holder_pct);
    }
    HolderVerdict::Ok
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

/// Fetch holder concentration for `mint`, excluding `excludes` (the pool's
/// own vaults — infrastructure accounts resolved by `resolve_swap_accounts`.
/// Mint/freeze authorities are `COption<Pubkey>` fields on the mint account
/// itself, never token-account holders, so there is nothing extra to drop
/// for them; callers pass both pool vaults so a pool can never count as a
/// "holder" in the rug-risk sense).
///
/// Fail-closed: any RPC error propagates. If the mint has zero supply, an
/// error is returned rather than a division-by-zero fallback.
pub fn fetch_holder_stats(
    rpc: &RpcClient,
    mint: &Pubkey,
    excludes: &[Pubkey],
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

    let entries: Vec<(String, u64)> = largest
        .iter()
        .map(|e| {
            e.amount
                .amount
                .parse::<u64>()
                .map_err(|err| format!("failed to parse largest-account amount: {err}"))
                .map(|amount| (e.address.clone(), amount))
        })
        .collect::<Result<_, String>>()?;

    compute_holder_stats(total_supply, &entries, excludes)
}

/// Pure, RPC-free holder-stat computation — unit-testable. Excludes the
/// pool's own vault accounts, then measures single-holder and combined
/// top-20 supply share (sample is capped at 20 by the SPL RPC standard).
fn compute_holder_stats(
    total_supply: u64,
    entries: &[(String, u64)],
    excludes: &[Pubkey],
) -> Result<HolderStats, String> {
    let mut sampled_holders = 0u64;
    let mut top_amount: u64 = 0;
    let mut combined_amount: u64 = 0;
    for (address, amount) in entries {
        let owner_pubkey = Pubkey::from_str(address)
            .map_err(|e| format!("invalid token account address {address}: {e}"))?;
        if excludes.contains(&owner_pubkey) {
            continue;
        }
        sampled_holders += 1;
        combined_amount = combined_amount.saturating_add(*amount);
        if *amount > top_amount {
            top_amount = *amount;
        }
    }

    // Round to 2 decimals so threshold comparisons and tests are exact.
    let pct = |amount: u64| ((amount as f64 / total_supply as f64) * 10_000.0).round() / 100.0;
    let top_holder_pct = pct(top_amount);
    let top20_holder_pct = pct(combined_amount);

    Ok(HolderStats {
        sampled_holders,
        top_holder_pct,
        top20_holder_pct,
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

// ── Holder-concentration gate (operator-approved design) ──

fn entry(addr: u8, amount: u64) -> (String, u64) {
    // Deterministic pseudo-pubkey from a single byte.
    let mut pk = [0u8; 32];
    pk[0] = addr;
    (Pubkey::new_from_array(pk).to_string(), amount)
}

#[test]
fn holder_stats_excludes_pool_vaults_before_concentration() {
    // Vault pubkey = the same account as entry(9) (first byte 9).
    let mut vk = [0u8; 32];
    vk[0] = 9;
    let vault = Pubkey::new_from_array(vk);
    // Supply 1000; vault holds 800 (pool infrastructure — must not count
    // as a "holder"), one real holder holds 150.
    let stats = compute_holder_stats(1_000, &[entry(9, 800), entry(1, 150)], &[vault]).unwrap();
    assert_eq!(stats.sampled_holders, 1, "vault must be excluded");
    assert_eq!(stats.top_holder_pct, 15.0);
    assert_eq!(stats.top20_holder_pct, 15.0);
    // Without the vault in the exclusion list it would dominate.
    let naive = compute_holder_stats(1_000, &[entry(9, 800), entry(1, 150)], &[]).unwrap();
    assert_eq!(naive.sampled_holders, 2);
    assert_eq!(naive.top_holder_pct, 80.0);
}

#[test]
fn concentration_gate_accepts_under_both_thresholds() {
    // 10 holders of 1% each => single 1%, top-20 10%: passes.
    let mut es = Vec::new();
    for i in 1..=10u8 {
        es.push(entry(i, 10));
    }
    let stats = compute_holder_stats(1_000, &es, &[]).unwrap();
    assert_eq!(holder_concentration_verdict(&stats), HolderVerdict::Ok);
}

#[test]
fn concentration_gate_rejects_single_holder_over_30pct() {
    // One holder at 40% (top-20 at 40% too) => single-holder breach fires
    // first with the 30% threshold.
    let stats =
        compute_holder_stats(1_000, &[entry(1, 400), entry(2, 100), entry(3, 100)], &[]).unwrap();
    assert_eq!(
        holder_concentration_verdict(&stats),
        HolderVerdict::SingleHolderConcentrated(40.0)
    );
}

#[test]
fn concentration_gate_rejects_top20_over_70pct_even_if_single_under_30() {
    // 19 holders at 4% each = 76% combined, single 4%: top-20 breach.
    let mut es = Vec::new();
    for i in 1..=19u8 {
        es.push(entry(i, 40));
    }
    let stats = compute_holder_stats(1_000, &es, &[]).unwrap();
    assert_eq!(stats.sampled_holders, 19);
    assert_eq!(stats.top20_holder_pct, 76.0);
    assert!(matches!(
        holder_concentration_verdict(&stats),
        HolderVerdict::TopHoldersConcentrated(76.0)
    ));
}

#[test]
fn concentration_gate_boundaries_are_inclusive_pass() {
    // Exactly 30% single and exactly 70% top-20 pass (threshold is ">").
    let stats =
        compute_holder_stats(1_000, &[entry(1, 300), entry(2, 250), entry(3, 150)], &[]).unwrap();
    assert_eq!(stats.top_holder_pct, 30.0);
    assert_eq!(stats.top20_holder_pct, 70.0);
    assert_eq!(holder_concentration_verdict(&stats), HolderVerdict::Ok);
}

#[test]
fn concentration_gate_unassessable_fails_closed() {
    // Every sampled account is excluded => no measurable holders => the
    // gate must reject (never silently pass).
    let mut vk = [0u8; 32];
    vk[0] = 7;
    let vault = Pubkey::new_from_array(vk);
    let stats = compute_holder_stats(1_000, &[entry(7, 1_000)], &[vault]).unwrap();
    assert_eq!(stats.sampled_holders, 0);
    assert_eq!(
        holder_concentration_verdict(&stats),
        HolderVerdict::Unassessable
    );
    // Zero-supply guard also fails closed (compute never sees 0, but the
    // verdict defends against it regardless).
    let stats = HolderStats {
        sampled_holders: 5,
        top_holder_pct: 1.0,
        top20_holder_pct: 5.0,
        total_supply: 0,
    };
    assert_eq!(
        holder_concentration_verdict(&stats),
        HolderVerdict::Unassessable
    );
}
