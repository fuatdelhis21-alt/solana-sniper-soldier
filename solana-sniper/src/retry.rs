//! Retry logic for transaction submission with exponential backoff.
//!
//! Implements BlockhashManager that caches and refreshes blockhashes,
//! and send_with_retry for robust transaction delivery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use tracing::{error, info, warn};

/// Manages recent blockhash with automatic refresh.
pub struct BlockhashManager {
    rpc: Arc<RpcClient>,
    cached_blockhash: Option<(Hash, Instant)>,
    refresh_interval: Duration,
}

impl BlockhashManager {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            cached_blockhash: None,
            refresh_interval: Duration::from_secs(30),
        }
    }

    /// Returns a cached blockhash, or fetches a fresh one if stale.
    pub fn get_or_refresh(&mut self) -> Result<Hash, Box<dyn std::error::Error>> {
        let now = Instant::now();
        if let Some((hash, ts)) = &self.cached_blockhash {
            if now.duration_since(*ts) < self.refresh_interval {
                return Ok(*hash);
            }
        }
        let hash = tokio::task::block_in_place(|| self.rpc.get_latest_blockhash())?;
        self.cached_blockhash = Some((hash, now));
        info!("blockhash refreshed: {hash}");
        Ok(hash)
    }

    /// Force fetch a fresh blockhash, bypassing cache.
    /// Prevents replay attacks by ensuring unique blockhash per iteration.
    pub fn force_refresh(&mut self) -> Result<Hash, Box<dyn std::error::Error>> {
        let hash = tokio::task::block_in_place(|| self.rpc.get_latest_blockhash())?;
        self.cached_blockhash = Some((hash, Instant::now()));
        info!("blockhash force-refreshed: {hash}");
        Ok(hash)
    }
}

/// Send a transaction with up to `max_retries` retries and exponential backoff.
pub fn send_with_retry(
    rpc: &RpcClient,
    tx: &Transaction,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let max_retries: u32 = 3;
    let mut attempt = 0u32;
    loop {
        match rpc.send_transaction(tx) {
            Ok(sig) => {
                info!("transaction sent: {sig}");
                return Ok(sig);
            }
            Err(e) => {
                attempt += 1;
                if attempt >= max_retries {
                    error!("send failed after {max_retries} retries: {e:#}");
                    return Err(Box::new(e));
                }
                let delay = Duration::from_millis(200 * 2u64.pow(attempt));
                warn!("send attempt {attempt} failed: {e:#}; retrying in {delay:?}");
                std::thread::sleep(delay);
            }
        }
    }
}
