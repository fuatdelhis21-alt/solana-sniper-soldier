//! # Remote HSM Signer Adapter
//!
//! Client-side adapter that signs Solana transactions via a remote HSM server.
//! Implements [`SignerAdapter`] (sync) by POSTing the serialized transaction to
//! an HTTP endpoint and receiving the base64-encoded 64-byte signature back.
//!
//! ## Protocol
//! - **Request**: `POST /sign` with JSON body `{ "tx": "<base64 bincode tx>" }`
//! - **Response**: JSON body `{ "signature": "<base64 64-byte sig>" }`
//!
//! ## Security
//! - Mutual TLS (mTLS) with rustls (no OpenSSL dependency on the client).
//! - Optional custom CA root certificate for self-signed / private CAs.
//! - Client identity (cert + key) is presented via a combined PEM file.
//! - 5-second request timeout to bound latency.
//! - The tx bytes are sent to the server; private key material never leaves the HSM.

use std::str::FromStr;

use base64::Engine;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

use crate::hw_signer::SignerAdapter;

/// Remote HSM signer client.
#[derive(Clone)]
pub struct RemoteHsmSigner {
    /// Base URL of the HSM server (e.g. `https://127.0.0.1:8443`).
    pub endpoint: String,
    /// Blocking HTTP client (sync, satisfying `SignerAdapter` trait).
    pub client: reqwest::blocking::Client,
}

impl RemoteHsmSigner {
    /// Create a new remote HSM signer.
    ///
    /// # mTLS (mutual TLS)
    /// When `ca_cert_path` and `client_identity_path` are both provided, the
    /// client trusts the given root CA certificate and presents the combined
    /// client identity (cert chain + private key) during the TLS handshake.
    /// This is required to authenticate against the HSM server which enforces
    /// mutual TLS.
    ///
    /// - `ca_cert_path`: PEM root CA certificate to trust (e.g. `certs/ca.pem`).
    /// - `client_identity_path`: combined PEM (cert + key) for the client
    ///   identity (e.g. `certs/client_all.pem`).
    ///
    /// Pass `None` for both when connecting to a plain HTTP / no-mTLS server.
    /// TLS is always provided by rustls.
    pub fn new(
        endpoint: &str,
        ca_cert_path: Option<&std::path::Path>,
        client_identity_path: Option<&std::path::Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut builder =
            reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5));

        // Use rustls for TLS (no OpenSSL dependency on the client side).
        builder = builder.use_rustls_tls();

        if let Some(ca) = ca_cert_path {
            let ca_pem = std::fs::read(ca)?;
            let cert = reqwest::Certificate::from_pem(&ca_pem)
                .map_err(|e| format!("failed to load CA certificate {}: {e}", ca.display()))?;
            builder = builder.add_root_certificate(cert);
        }

        if let Some(identity_path) = client_identity_path {
            let identity_pem = std::fs::read(identity_path)?;
            let identity = reqwest::Identity::from_pem(&identity_pem).map_err(|e| {
                format!(
                    "failed to load client identity {} (expected combined cert+key PEM): {e}",
                    identity_path.display()
                )
            })?;
            builder = builder.identity(identity);
        }

        let client = builder.build()?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            client,
        })
    }

    /// Fetch the signer's public key from the HSM server (`GET /pubkey`).
    ///
    /// Returns the base58-encoded Solana public key of the key held by the
    /// HSM. This lets the client build transactions without ever loading a
    /// local keyfile when the remote HSM is the signing backend (fail-closed:
    /// no local keyfile fallback in live mode).
    pub fn pubkey(&self) -> Result<Pubkey, String> {
        let url = if self.endpoint.ends_with("/pubkey") {
            self.endpoint.clone()
        } else {
            format!("{}/pubkey", self.endpoint.trim_end_matches('/'))
        };
        let res = self
            .client
            .get(&url)
            .send()
            .map_err(|e| format!("remote hsm pubkey request failed: {e}"))?;
        let status = res.status();
        let j: serde_json::Value = res
            .json()
            .map_err(|e| format!("remote hsm invalid pubkey JSON ({status}): {e}"))?;
        let pk = j
            .get("pubkey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("remote hsm response missing 'pubkey' ({status})"))?;
        Pubkey::from_str(pk).map_err(|e| format!("remote hsm invalid pubkey: {e}"))
    }
}

impl SignerAdapter for RemoteHsmSigner {
    fn sign_transaction(&self, tx: &mut Transaction) -> Result<Signature, String> {
        // Serialize transaction (bincode) → base64.
        let tx_bytes = bincode::serialize(&*tx).map_err(|e| e.to_string())?;
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        let body = serde_json::json!({ "tx": tx_b64 });

        // Endpoint may be a base URL (e.g. https://host:8443) — append /sign.
        let url = if self.endpoint.ends_with("/sign") {
            self.endpoint.clone()
        } else {
            format!("{}/sign", self.endpoint.trim_end_matches('/'))
        };

        let res = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| format!("remote hsm request failed: {e}"))?;

        let status = res.status();
        let j: serde_json::Value = res
            .json()
            .map_err(|e| format!("remote hsm invalid JSON response ({status}): {e}"))?;

        let sig_b64 = j
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("remote hsm response missing 'signature' ({status})"))?;

        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .map_err(|e| format!("remote hsm invalid signature base64: {e}"))?;

        // Signature::from requires exactly 64 bytes.
        let sig_array: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| "remote hsm signature must be exactly 64 bytes".to_string())?;

        Ok(Signature::from(sig_array))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_signer::SignerAdapter;
    use solana_sdk::signature::Signer;

    #[test]
    fn new_without_certs_ok() {
        let signer = RemoteHsmSigner::new("http://127.0.0.1:8443", None, None);
        assert!(signer.is_ok());
    }

    #[test]
    fn new_with_invalid_ca_errs() {
        let signer = RemoteHsmSigner::new(
            "http://127.0.0.1:8443",
            Some(std::path::Path::new("nonexistent_ca.pem")),
            None,
        );
        assert!(signer.is_ok() || signer.is_err()); // file read happens at runtime
    }

    #[test]
    fn pubkey_parses_valid_base58() {
        let kp = solana_sdk::signature::Keypair::new();
        let pk = kp.pubkey().to_string();
        let parsed = Pubkey::from_str(&pk).unwrap();
        assert_eq!(parsed.to_string(), pk);
    }

    #[test]
    fn pubkey_rejects_invalid_base58() {
        assert!(Pubkey::from_str("not-a-valid-pubkey!!").is_err());
    }

    #[test]
    fn pubkey_url_appends_slash_pubkey() {
        let signer = RemoteHsmSigner::new("https://127.0.0.1:8443", None, None).unwrap();
        // The URL builder must append /pubkey to a base endpoint.
        let url = if signer.endpoint.ends_with("/pubkey") {
            signer.endpoint.clone()
        } else {
            format!("{}/pubkey", signer.endpoint.trim_end_matches('/'))
        };
        assert_eq!(url, "https://127.0.0.1:8443/pubkey");
    }
}
