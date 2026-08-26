//! Decision record — deterministic snapshot of every trade decision for replay/audit.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Snapshot of all inputs that went into a trade decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Unix timestamp (ms) when the decision was made
    pub decision_time_ms: u64,
    /// Strategy ID or name
    pub strategy: String,
    /// Protocol (e.g. "raydium_v4", "orca_whirlpool")
    pub protocol: String,
    /// Pool public key
    pub pool_id: String,
    /// Token mint addresses involved
    pub token_in: String,
    pub token_out: String,
    /// Amount in (lamports / smallest unit)
    pub amount_in: u64,
    /// Minimum amount out (after slippage)
    pub min_amount_out: u64,
    /// Current sqrt price (Q64.64) at decision time
    pub sqrt_price: String,
    /// Current liquidity
    pub liquidity: String,
    /// Recent blockhash used for the transaction
    pub blockhash: String,
    /// Transaction signature (if sent, else placeholder)
    pub signature: String,
    /// Mode: "simulation", "dry_run", or "live"
    pub mode: String,
    /// Additional arbitrary context
    pub context: serde_json::Value,
}

impl DecisionRecord {
    pub fn new(strategy: &str, protocol: &str) -> Self {
        Self {
            decision_time_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            strategy: strategy.to_string(),
            protocol: protocol.to_string(),
            pool_id: String::new(),
            token_in: String::new(),
            token_out: String::new(),
            amount_in: 0,
            min_amount_out: 0,
            sqrt_price: String::new(),
            liquidity: String::new(),
            blockhash: String::new(),
            signature: String::new(),
            mode: "simulation".to_string(),
            context: serde_json::Value::Null,
        }
    }

    /// Save to a JSONL file.
    pub fn save(&self, base_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        let dir = base_dir.join("decisions");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("decisions.jsonl");
        let line = serde_json::to_string(self)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{line}")?;
        Ok(())
    }
}
