use crate::{MarketDataHandler, PriceQuote};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub struct MockMarketProvider {
    prices: Arc<RwLock<HashMap<String, PriceQuote>>>,
}

impl MockMarketProvider {
    pub fn new() -> Self {
        Self {
            prices: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn update_price(&self, symbol: &str, bid: f64, ask: f64) {
        let quote = PriceQuote {
            symbol: symbol.to_string(),
            bid,
            ask,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        };
        self.prices.write().insert(symbol.to_string(), quote);
    }
}

impl MarketDataHandler for MockMarketProvider {
    fn start_stream(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn get_latest_price(&self, symbol: &str) -> Option<PriceQuote> {
        self.prices.read().get(symbol).cloned()
    }
}
