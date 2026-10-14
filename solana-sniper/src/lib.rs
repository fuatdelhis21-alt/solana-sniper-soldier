pub mod amm;
pub mod config;
pub mod decision;
pub mod discovery;
pub mod executor;
pub mod hw_signer;
pub mod integration;
pub mod jito;
pub mod ledger_signer;
pub mod marketdata;
pub mod metrics;
pub mod onchain_risk;
pub mod pool_discovery;
pub mod position;
pub mod redact;
pub mod remote_hsm;
pub mod retry;
pub mod risk;
pub mod signal;
pub mod sim_quote;
pub mod strategy;
pub mod tx;

/// The remote HSM's signing policy, compiled into the tests so the
/// transactions the swap builder really produces are checked against it.
#[cfg(test)]
#[path = "../../tools/hsm_server/policy.rs"]
#[allow(dead_code)]
mod hsm_policy;
