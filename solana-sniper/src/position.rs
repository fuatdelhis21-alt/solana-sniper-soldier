//! # Live position tracking — restart-safe record of the one open position
//!
//! The risk manager only counts open positions; to actually EXIT a position
//! (stop-loss / take-profit sell) the live loop needs to know what was bought,
//! where, how much it cost and how many tokens are held. That record lives in
//! `<data_dir>/live_position.json` and is written immediately after a
//! confirmed entry and removed immediately after a confirmed exit, so a
//! restart resumes managing the same position instead of orphaning it.
//!
//! Balances are measured on-chain before/after each swap, so cost, token
//! amount and realized P&L are real numbers (fees included), never quotes.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use crate::amm::account_resolver::resolve_user_ata;
use crate::amm::raydium_v4::NATIVE_MINT;

const POSITION_FILE: &str = "live_position.json";

/// One open live position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePosition {
    pub pool_id: String,
    /// Mint spent on entry (and received back on exit), e.g. wSOL.
    pub input_mint: String,
    /// Mint bought on entry (and sold on exit).
    pub output_mint: String,
    /// Q64.64 sqrt price at entry (string: JSON has no u128).
    pub entry_sqrt_price: String,
    /// Position size the risk manager accounted for (lamports).
    pub position_size_lamports: u64,
    /// Real input-side balance spent by the entry (fees included).
    pub cost_input_units: u64,
    /// Real amount of `output_mint` received by the entry.
    pub token_amount: u64,
    pub entry_signature: String,
    pub opened_at_ms: u64,
}

impl LivePosition {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(POSITION_FILE)
    }

    /// Load the persisted position. `Ok(None)` = no open position. A file
    /// that exists but cannot be parsed is an error (fail-closed).
    pub fn load(data_dir: &Path) -> Result<Option<Self>, String> {
        let path = Self::path(data_dir);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| format!("corrupt {}: {e}", path.display()))
    }

    /// Persist atomically (write temp file, then rename).
    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        let path = Self::path(data_dir);
        let tmp = data_dir.join(format!("{POSITION_FILE}.tmp"));
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("cannot rename to {}: {e}", path.display()))
    }

    /// Remove the record after a confirmed exit.
    pub fn clear(data_dir: &Path) -> Result<(), String> {
        let path = Self::path(data_dir);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        }
    }

    pub fn entry_sqrt(&self) -> u128 {
        self.entry_sqrt_price.parse().unwrap_or(0)
    }

    /// True if this record belongs to the given pool/mint configuration.
    pub fn matches(&self, pool_id: &str, input_mint: &str, output_mint: &str) -> bool {
        self.pool_id == pool_id && self.input_mint == input_mint && self.output_mint == output_mint
    }
}

/// Realized P&L in input units: what the exit returned minus what the entry
/// cost. Negative = loss.
pub fn realized_pnl(cost_input_units: u64, proceeds_input_units: u64) -> i64 {
    proceeds_input_units as i64 - cost_input_units as i64
}

/// SPL token account amount (bytes 64..72). A missing account is a zero
/// balance (the ATA is created on the first swap); an RPC error is an error.
pub fn fetch_token_balance(rpc: &RpcClient, account: &Pubkey) -> Result<u64, String> {
    let resp = rpc
        .get_account_with_commitment(account, CommitmentConfig::confirmed())
        .map_err(|e| format!("token balance fetch failed for {account}: {e}"))?;
    match resp.value {
        None => Ok(0),
        Some(acc) => {
            if acc.data.len() < 72 {
                return Err(format!("{account} is not an SPL token account"));
            }
            Ok(u64::from_le_bytes(acc.data[64..72].try_into().unwrap()))
        }
    }
}

/// The owner's spendable balance of `mint` in raw units. For the native mint
/// this is lamports + any wSOL ATA balance (the swap wraps/unwraps SOL), so
/// before/after deltas capture the full cost incl. network fees and rent.
pub fn fetch_mint_balance(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64, String> {
    let ata = resolve_user_ata(owner, mint);
    let token = fetch_token_balance(rpc, &ata)?;
    if *mint == Pubkey::from_str(NATIVE_MINT).expect("valid native mint") {
        let lamports = rpc
            .get_balance_with_commitment(owner, CommitmentConfig::confirmed())
            .map_err(|e| format!("SOL balance fetch failed for {owner}: {e}"))?
            .value;
        Ok(lamports.saturating_add(token))
    } else {
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pos_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> LivePosition {
        LivePosition {
            pool_id: "pool".into(),
            input_mint: NATIVE_MINT.into(),
            output_mint: "mint".into(),
            entry_sqrt_price: (1u128 << 64).to_string(),
            position_size_lamports: 10_000_000,
            cost_input_units: 10_005_000,
            token_amount: 42,
            entry_signature: "sig".into(),
            opened_at_ms: 1,
        }
    }

    #[test]
    fn save_load_clear_roundtrip() {
        let d = tmp_dir();
        assert_eq!(LivePosition::load(&d).unwrap(), None);
        let p = sample();
        p.save(&d).unwrap();
        let loaded = LivePosition::load(&d).unwrap().unwrap();
        assert_eq!(loaded, p);
        assert_eq!(loaded.entry_sqrt(), 1u128 << 64);
        LivePosition::clear(&d).unwrap();
        assert_eq!(LivePosition::load(&d).unwrap(), None);
        // Clearing twice is fine.
        LivePosition::clear(&d).unwrap();
    }

    #[test]
    fn corrupt_file_is_fail_closed_error() {
        let d = tmp_dir();
        std::fs::write(d.join(POSITION_FILE), "{not json").unwrap();
        assert!(LivePosition::load(&d).is_err());
    }

    #[test]
    fn matches_checks_pool_and_mints() {
        let p = sample();
        assert!(p.matches("pool", NATIVE_MINT, "mint"));
        assert!(!p.matches("other", NATIVE_MINT, "mint"));
    }

    #[test]
    fn realized_pnl_sign() {
        assert_eq!(realized_pnl(10_000, 11_000), 1_000);
        assert_eq!(realized_pnl(10_000, 9_000), -1_000);
    }
}
