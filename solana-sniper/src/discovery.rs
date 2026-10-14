//! # DexScreener Discovery (Advisory Only)
//!
//! Optional cross-check against the public DexScreener API for a pool's
//! liquidity/volume/market-cap figures.
//!
//! ## Safety
//! This module is **never** part of the fail-closed trading gate. DexScreener
//! is a third-party service outside our control: it can be slow, wrong,
//! rate-limited, or unavailable. All real risk decisions (liquidity, holder
//! concentration, blocklist) must come from `onchain_risk`, which reads
//! directly from the Solana RPC.
//!
//! Any error from this module is logged as a warning by the caller and must
//! never abort or block a trading decision.

use serde::Deserialize;

const DEXSCREENER_BASE_URL: &str = "https://api.dexscreener.com/latest/dex/pairs/solana";

/// Advisory snapshot of a pool as reported by DexScreener. All fields are
/// best-effort and optional because the API's coverage of a given pair is
/// not guaranteed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DexScreenerSnapshot {
    pub liquidity_usd: Option<f64>,
    pub fdv: Option<f64>,
    pub volume_24h_usd: Option<f64>,
    pub price_usd: Option<String>,
    pub pair_created_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerResponse {
    pairs: Option<Vec<DexScreenerPairRaw>>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerPairRaw {
    #[serde(rename = "chainId")]
    chain_id: Option<String>,
    #[serde(rename = "pairAddress")]
    pair_address: Option<String>,
    #[serde(rename = "priceUsd")]
    price_usd: Option<String>,
    liquidity: Option<DexScreenerLiquidity>,
    fdv: Option<f64>,
    volume: Option<DexScreenerVolume>,
    #[serde(rename = "pairCreatedAt")]
    pair_created_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerLiquidity {
    usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerVolume {
    h24: Option<f64>,
}

/// Fetch an advisory snapshot for `pair_address` from DexScreener.
///
/// This performs a blocking HTTP GET with a short timeout so a slow/down
/// third party can never stall the trading loop for long. Errors are
/// returned to the caller as `Err(String)` — the caller must treat this as
/// advisory-only and never let it affect the fail-closed risk gate.
pub fn fetch_snapshot(pair_address: &str) -> Result<DexScreenerSnapshot, String> {
    let url = format!("{DEXSCREENER_BASE_URL}/{pair_address}");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build dexscreener http client: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("dexscreener request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("dexscreener returned status {}", resp.status()));
    }

    let body: DexScreenerResponse = resp
        .json()
        .map_err(|e| format!("failed to parse dexscreener response: {e}"))?;

    let pair = select_pair(body.pairs.unwrap_or_default(), pair_address)?;
    Ok(parse_pair(pair))
}

/// The response entry for exactly `pair_address` on Solana. The endpoint can
/// return several or unrelated pairs; taking the first one blindly could
/// attribute another pool's figures to this one.
fn select_pair(
    pairs: Vec<DexScreenerPairRaw>,
    pair_address: &str,
) -> Result<DexScreenerPairRaw, String> {
    pairs
        .into_iter()
        .find(|p| {
            p.chain_id.as_deref() == Some("solana")
                && p.pair_address.as_deref() == Some(pair_address)
        })
        .ok_or_else(|| format!("dexscreener has no Solana pair data for {pair_address}"))
}

fn parse_pair(pair: DexScreenerPairRaw) -> DexScreenerSnapshot {
    DexScreenerSnapshot {
        liquidity_usd: pair.liquidity.and_then(|l| l.usd),
        fdv: pair.fdv,
        volume_24h_usd: pair.volume.and_then(|v| v.h24),
        price_usd: pair.price_usd,
        pair_created_at: pair.pair_created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_dexscreener_fixture() {
        let json = r#"{
            "pairs": [{
                "priceUsd": "0.00012345",
                "liquidity": {"usd": 54321.5},
                "fdv": 987654.0,
                "volume": {"h24": 12345.6},
                "pairCreatedAt": 1700000000000
            }]
        }"#;
        let body: DexScreenerResponse = serde_json::from_str(json).unwrap();
        let pair = body.pairs.unwrap().remove(0);
        let snapshot = parse_pair(pair);

        assert_eq!(snapshot.liquidity_usd, Some(54321.5));
        assert_eq!(snapshot.fdv, Some(987654.0));
        assert_eq!(snapshot.volume_24h_usd, Some(12345.6));
        assert_eq!(snapshot.price_usd.as_deref(), Some("0.00012345"));
        assert_eq!(snapshot.pair_created_at, Some(1700000000000));
    }

    #[test]
    fn parses_partial_fixture_with_missing_fields() {
        let json = r#"{"pairs": [{}]}"#;
        let body: DexScreenerResponse = serde_json::from_str(json).unwrap();
        let pair = body.pairs.unwrap().remove(0);
        let snapshot = parse_pair(pair);

        assert_eq!(snapshot.liquidity_usd, None);
        assert_eq!(snapshot.fdv, None);
        assert_eq!(snapshot.volume_24h_usd, None);
        assert_eq!(snapshot.price_usd, None);
        assert_eq!(snapshot.pair_created_at, None);
    }

    #[test]
    fn selects_only_the_requested_solana_pair() {
        let json = r#"{"pairs": [
            {"chainId": "ethereum", "pairAddress": "PAIR", "fdv": 1.0},
            {"chainId": "solana", "pairAddress": "OTHER", "fdv": 2.0},
            {"chainId": "solana", "pairAddress": "PAIR", "fdv": 3.0}
        ]}"#;
        let body: DexScreenerResponse = serde_json::from_str(json).unwrap();
        let pair = select_pair(body.pairs.unwrap(), "PAIR").unwrap();
        assert_eq!(parse_pair(pair).fdv, Some(3.0));

        let body: DexScreenerResponse =
            serde_json::from_str(r#"{"pairs": [{"chainId": "solana", "pairAddress": "OTHER"}]}"#)
                .unwrap();
        assert!(select_pair(body.pairs.unwrap(), "PAIR").is_err());
        assert!(select_pair(vec![], "PAIR").is_err());
        let body: DexScreenerResponse = serde_json::from_str(r#"{"pairs": null}"#).unwrap();
        assert!(select_pair(body.pairs.unwrap_or_default(), "PAIR").is_err());
    }
}
