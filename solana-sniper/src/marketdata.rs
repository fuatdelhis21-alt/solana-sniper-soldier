//! # Pool Price Feed
//!
//! Bridges the `hft-marketdata` layer into the live strategy loop by polling
//! the Raydium CLMM pool state over RPC and exposing the latest on-chain
//! `sqrt_price` / `liquidity` as a [`PriceQuote`].
//!
//! ## Safety
//! - Fail-closed: a failed refresh propagates the error and retains the last
//!   good snapshot; the strategy never sees a fabricated price.
//! - No secrets are logged; only public pool/price data is exposed.

use crate::amm::account_resolver::{fetch_pool_state, ResolvedPool};
use hft_marketdata::{MarketDataHandler, PriceQuote};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::sync::{Arc, Mutex};

/// Polls a Raydium CLMM pool state over RPC and caches the latest price.
pub struct PoolPriceFeed {
    rpc: Arc<RpcClient>,
    pool_id: Pubkey,
    program_id: Pubkey,
    last: Mutex<Option<ResolvedPool>>,
    /// Unix ms timestamp of the last successful `refresh()`. `None` means
    /// no snapshot has ever been fetched — used by `age_ms`/`is_stale` to
    /// fail closed on data that was never actually retrieved.
    last_refreshed_ms: Mutex<Option<u128>>,
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl PoolPriceFeed {
    pub fn new(rpc: Arc<RpcClient>, pool_id: Pubkey, program_id: Pubkey) -> Self {
        Self {
            rpc,
            pool_id,
            program_id,
            last: Mutex::new(None),
            last_refreshed_ms: Mutex::new(None),
        }
    }

    /// Refresh the cached pool state from the RPC. Fail-closed: any error
    /// propagates and the previous snapshot is retained.
    pub fn refresh(&self) -> Result<ResolvedPool, String> {
        let pool = fetch_pool_state(&self.rpc, &self.pool_id)?;
        *self.last.lock().unwrap() = Some(pool.clone());
        *self.last_refreshed_ms.lock().unwrap() = Some(now_ms());
        Ok(pool)
    }

    /// Age (ms) of the cached snapshot, or `None` if nothing has ever been
    /// fetched (treated as maximally stale by callers).
    pub fn age_ms(&self) -> Option<u128> {
        let ts = (*self.last_refreshed_ms.lock().unwrap())?;
        Some(now_ms().saturating_sub(ts))
    }

    /// Fail-closed staleness check: missing data (never fetched) is always
    /// stale, regardless of `max_age_ms`.
    pub fn is_stale(&self, max_age_ms: u64) -> bool {
        match self.age_ms() {
            Some(age) => age > max_age_ms as u128,
            None => true,
        }
    }

    /// The most recent successfully-fetched pool snapshot.
    pub fn latest(&self) -> Option<ResolvedPool> {
        self.last.lock().unwrap().clone()
    }

    /// The latest on-chain sqrt price (Q64.64), if a snapshot is cached.
    pub fn latest_sqrt_price(&self) -> Option<u128> {
        self.latest().map(|p| p.sqrt_price_x64)
    }

    /// The latest on-chain liquidity, if a snapshot is cached.
    pub fn latest_liquidity(&self) -> Option<u128> {
        self.latest().map(|p| p.liquidity)
    }

    /// Convert the latest pool snapshot to a [`PriceQuote`] (token1/token0).
    pub fn to_quote(&self) -> Option<PriceQuote> {
        let pool = self.latest()?;
        // Use the actual last-fetch timestamp (not "now") so a downstream
        // staleness check reflects when the data was really retrieved.
        let timestamp_ms = (*self.last_refreshed_ms.lock().unwrap()).unwrap_or(0);
        Some(PriceQuote {
            symbol: format!("{}/{}", pool.token_mint_0, pool.token_mint_1),
            bid: pool.price(),
            ask: pool.price(),
            timestamp_ms,
        })
    }
}

impl MarketDataHandler for PoolPriceFeed {
    fn start_stream(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.refresh().map(|_| ()).map_err(|e| e.into())
    }

    fn get_latest_price(&self, symbol: &str) -> Option<PriceQuote> {
        let q = self.to_quote()?;
        if q.symbol == symbol {
            Some(q)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_starts_empty_and_fails_closed() {
        let rpc = Arc::new(RpcClient::new("http://127.0.0.1:8899".to_string()));
        let feed = PoolPriceFeed::new(rpc, Pubkey::new_unique(), Pubkey::new_unique());
        // No snapshot yet: no price, no quote.
        assert!(feed.latest_sqrt_price().is_none());
        assert!(feed.to_quote().is_none());
        // Refresh against a dead RPC must fail closed (Err), not panic.
        assert!(feed.refresh().is_err());
    }

    #[test]
    fn never_fetched_is_always_stale() {
        let rpc = Arc::new(RpcClient::new("http://127.0.0.1:8899".to_string()));
        let feed = PoolPriceFeed::new(rpc, Pubkey::new_unique(), Pubkey::new_unique());
        assert!(feed.age_ms().is_none());
        // Even a huge max age must not make "never fetched" pass — fail-closed.
        assert!(feed.is_stale(u64::MAX));
    }
}
