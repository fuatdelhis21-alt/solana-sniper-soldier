//! Jito bundle integration — MEV-protected bundle submission with RPC fallback.
//!
//! ## Modes
//! - **Dry-run / simulation** (`dry_run = true`): builds and validates the
//!   bundle payload but never POSTs it to the Block Engine. Used for Devnet
//!   testing without sending real bundles.
//! - **Live** (`dry_run = false`): POSTs the bundle to the Jito Block Engine
//!   relayer. If the bundle submission fails, the caller can fall back to a
//!   normal RPC submission via `send_with_retry`.
//!
//! ## Safety
//! - Fail-closed: a bundle is only sent when the caller explicitly opts into
//!   live mode. Dry-run never touches the network.
//! - Every submission attempt (or dry-run validation) is logged for audit.

use solana_sdk::transaction::Transaction;
use tracing::{error, info, warn};

/// Jito Block Engine relayer endpoint (mainnet).
pub const JITO_MAINNET_BLOCK_ENGINE: &str = "https://mainnet.block-engine.jito.wtf";
/// Jito Block Engine relayer endpoint (devnet).
pub const JITO_DEVNET_BLOCK_ENGINE: &str = "https://devnet.block-engine.jito.wtf";

/// A prepared Jito bundle payload.
pub struct JitoBundle {
    pub transactions: Vec<Transaction>,
    pub tip_lamports: u64,
}

impl JitoBundle {
    pub fn new(txs: Vec<Transaction>, tip_lamports: u64) -> Self {
        Self {
            transactions: txs,
            tip_lamports,
        }
    }
}

/// Jito Block Engine client for sending MEV-protected bundles.
pub struct JitoClient {
    pub endpoint: String,
    /// When true, `send_bundle` validates the payload but never POSTs it.
    pub dry_run: bool,
    http: reqwest::Client,
}

impl JitoClient {
    pub fn new(endpoint: &str, dry_run: bool) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            dry_run,
            http: reqwest::Client::new(),
        }
    }

    /// Serialize the bundle transactions into the Jito `bundleTransactions`
    /// wire format (base64-encoded signed transaction bytes).
    fn serialize_bundle(&self, bundle: &JitoBundle) -> Result<Vec<String>, String> {
        bundle
            .transactions
            .iter()
            .map(|tx| {
                bincode::serialize(tx)
                    .map(|bytes| base64::encode(&bytes))
                    .map_err(|e| format!("failed to serialize transaction: {e}"))
            })
            .collect()
    }

    /// Send a bundle via the Jito Block Engine.
    ///
    /// In dry-run mode this validates the payload and returns a synthetic
    /// bundle ID without any network I/O. In live mode it POSTs to
    /// `{endpoint}/api/v1/bundles`.
    pub async fn send_bundle(&self, bundle: &JitoBundle) -> Result<String, String> {
        let txs = self.serialize_bundle(bundle)?;
        if txs.is_empty() {
            return Err("cannot send an empty bundle".into());
        }

        if self.dry_run {
            info!(
                target: "jito",
                endpoint = %self.endpoint,
                tx_count = txs.len(),
                tip = bundle.tip_lamports,
                "Jito bundle DRY-RUN — validated, not sent"
            );
            return Ok("dry_run_bundle_placeholder".to_string());
        }

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [txs]
        });

        let url = format!("{}/api/v1/bundles", self.endpoint);
        info!(
            target: "jito",
            endpoint = %self.endpoint,
            tx_count = txs.len(),
            tip = bundle.tip_lamports,
            "Jito bundle gönderiliyor"
        );

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("jito request failed: {e}"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("jito response read failed: {e}"))?;

        if !status.is_success() {
            error!(target: "jito", status = %status, body = %text, "Jito bundle rejected");
            return Err(format!("jito bundle rejected: HTTP {status}: {text}"));
        }

        // Jito returns {"result": "<bundle_id>"} on success.
        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("jito response parse failed: {e}"))?;
        let bundle_id = parsed
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("jito response missing result: {text}"))?;

        info!(target: "jito", bundle_id = %bundle_id, "Jito bundle accepted");
        Ok(bundle_id.to_string())
    }
}

/// Send a bundle via Jito's BlockEngine relayer.
///
/// This is a convenience wrapper around `JitoClient::send_bundle`. In dry-run
/// mode it never touches the network.
pub async fn send_bundle(
    endpoint: &str,
    dry_run: bool,
    bundle: &JitoBundle,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = JitoClient::new(endpoint, dry_run);
    Ok(client.send_bundle(bundle).await?)
}

/// RPC fallback: if the Jito bundle submission fails, fall back to sending the
/// transactions individually via the standard RPC with retry.
///
/// Returns the first successfully submitted signature. If all submissions
/// fail, returns the last error.
pub async fn send_with_rpc_fallback(
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    txs: &[Transaction],
) -> Result<solana_sdk::signature::Signature, Box<dyn std::error::Error>> {
    if txs.is_empty() {
        return Err("no transactions to send via RPC fallback".into());
    }
    let mut last_err: Option<Box<dyn std::error::Error>> = None;
    for tx in txs {
        match crate::retry::send_with_retry(rpc, tx) {
            Ok(sig) => {
                info!(target: "jito", sig = %sig, "RPC fallback submission succeeded");
                return Ok(sig);
            }
            Err(e) => {
                warn!(target: "jito", err = %e, "RPC fallback submission failed");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "RPC fallback failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;
    use solana_sdk::message::Message;
    use solana_sdk::signature::{Keypair, Signer};
    use solana_sdk::system_instruction;

    fn sample_tx() -> Transaction {
        let kp = Keypair::new();
        let ix = system_instruction::transfer(&kp.pubkey(), &kp.pubkey(), 1_000);
        let msg = Message::new(&[ix], Some(&kp.pubkey()));
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[&kp], Hash::default());
        tx
    }

    #[tokio::test]
    async fn dry_run_returns_placeholder_without_network() {
        let client = JitoClient::new(JITO_DEVNET_BLOCK_ENGINE, true);
        let bundle = JitoBundle::new(vec![sample_tx()], 1_000);
        let id = client.send_bundle(&bundle).await.unwrap();
        assert_eq!(id, "dry_run_bundle_placeholder");
    }

    #[tokio::test]
    async fn empty_bundle_rejected() {
        let client = JitoClient::new(JITO_DEVNET_BLOCK_ENGINE, true);
        let bundle = JitoBundle::new(vec![], 0);
        assert!(client.send_bundle(&bundle).await.is_err());
    }

    #[test]
    fn serialize_bundle_produces_base64() {
        let client = JitoClient::new(JITO_DEVNET_BLOCK_ENGINE, true);
        let bundle = JitoBundle::new(vec![sample_tx()], 1_000);
        let txs = client.serialize_bundle(&bundle).unwrap();
        assert_eq!(txs.len(), 1);
        // base64 decode should succeed and yield non-empty bytes.
        let decoded = base64::decode(&txs[0]).unwrap();
        assert!(!decoded.is_empty());
    }

    #[test]
    fn bundle_holds_transactions_and_tip() {
        let bundle = JitoBundle::new(vec![sample_tx()], 5_000);
        assert_eq!(bundle.transactions.len(), 1);
        assert_eq!(bundle.tip_lamports, 5_000);
    }
}
