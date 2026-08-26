//! Configuration management — loads TOML config + .env override.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level HFT platform configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HftConfig {
    pub rpc: RpcConfig,
    pub risk: RiskConfigSection,
    pub trading: TradingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    pub endpoint: String,
    pub ws_endpoint: String,
    pub commitment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfigSection {
    pub max_trade_size_sol: f64,
    pub max_slippage_bps: u64,
    pub daily_loss_limit_sol: f64,
    pub circuit_breaker_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub strategy: String,
    pub protocol: String,
    pub priority_microlamports: u64,
    pub compute_units: u32,
    pub jito_tip_lamports: u64,
}

impl Default for HftConfig {
    fn default() -> Self {
        Self {
            rpc: RpcConfig {
                endpoint: "https://api.devnet.solana.com".into(),
                ws_endpoint: "wss://api.devnet.solana.com".into(),
                commitment: "confirmed".into(),
            },
            risk: RiskConfigSection {
                max_trade_size_sol: 0.1,
                max_slippage_bps: 100,
                daily_loss_limit_sol: 1.0,
                circuit_breaker_minutes: 5,
            },
            trading: TradingConfig {
                strategy: "simple_snipe".into(),
                protocol: "raydium_v4".into(),
                priority_microlamports: 10_000,
                compute_units: 200_000,
                jito_tip_lamports: 0,
            },
        }
    }
}

impl HftConfig {
    /// Load config from a TOML file, falling back to defaults + env.
    pub fn load(path: Option<&PathBuf>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut cfg = HftConfig::default();

        if let Some(p) = path {
            if p.exists() {
                let content = std::fs::read_to_string(p)?;
                let file_cfg: HftConfig = toml::from_str(&content)?;
                cfg = file_cfg;
            }
        }

        // Environment overrides
        if let Ok(v) = std::env::var("SOLANA_RPC_ENDPOINT") {
            cfg.rpc.endpoint = v;
        }
        if let Ok(v) = std::env::var("SOLANA_WS_ENDPOINT") {
            cfg.rpc.ws_endpoint = v;
        }
        if let Ok(v) = std::env::var("MAX_TRADE_SIZE_SOL") {
            cfg.risk.max_trade_size_sol = v.parse()?;
        }
        if let Ok(v) = std::env::var("MAX_SLIPPAGE_BPS") {
            cfg.risk.max_slippage_bps = v.parse()?;
        }
        if let Ok(v) = std::env::var("DAILY_LOSS_LIMIT_SOL") {
            cfg.risk.daily_loss_limit_sol = v.parse()?;
        }

        Ok(cfg)
    }
}
