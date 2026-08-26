//! # Remote HSM Signer Server (scaffold)
//!
//! Exposes a `POST /sign` HTTP endpoint that accepts a serialized Solana
//! transaction and returns a base64-encoded 64-byte signature.
//!
//! ## Protocol
//! - **Request**: JSON `{ "tx": "<base64 bincode Transaction>" }`
//! - **Response**: JSON `{ "signature": "<base64 64-byte signature>" }`
//!
//! ## Key source
//! The signing key is loaded from the `HSM_KEY_B64` environment variable
//! (base64-encoded 64-byte private key bytes). If unset/invalid, an ephemeral
//! keypair is generated at startup (dev/test scaffold only).
//!
//! ## mTLS
//! When `server.pem`, `server.key` and `ca.pem` exist in the certs directory,
//! the server enforces mutual TLS: clients must present a certificate signed
//! by the same CA (`ca.pem`) before any request is served.
//!
//! ## Usage
//! ```bash
//! cargo run -p tools-hsm-server -- --certs tools/hsm_server/certs --log-file logs/hsm_audit.log
//! ```

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use warp::Filter;

/// Sign request body.
#[derive(Deserialize)]
struct SignRequest {
    /// Base64-encoded bincode serialized `Transaction`.
    tx: String,
}

/// Sign response body.
#[derive(Serialize)]
struct SignResponse {
    /// Base64-encoded 64-byte Ed25519 signature.
    signature: String,
}

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Simple unique request id (ms + process-local counter).
fn request_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", now, seq)
}

/// Append-only audit logger for signing requests.
struct AuditLogger {
    file: Mutex<Option<File>>,
}

impl AuditLogger {
    fn new(path: Option<PathBuf>) -> Self {
        let file = path.and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        Self {
            file: Mutex::new(file),
        }
    }

    fn log(&self, line: &str) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}

/// Load signer from `HSM_KEY_B64`, otherwise generate an ephemeral keypair.
fn load_or_create_signer() -> Keypair {
    if let Ok(key_b64) = std::env::var("HSM_KEY_B64") {
        if let Ok(bytes) = general_purpose::STANDARD.decode(key_b64) {
            if let Ok(arr) = <[u8; 64]>::try_from(bytes.as_slice()) {
                if let Ok(kp) = Keypair::from_bytes(&arr) {
                    return kp;
                }
            }
        }
        println!("[hsm_server] HSM_KEY_B64 invalid; using ephemeral keypair");
    }
    let kp = Keypair::new();
    println!("[hsm_server] ephemeral signer pubkey: {}", kp.pubkey());
    kp
}

/// Sign a base64-encoded bincode `Transaction`, returning the base64 signature.
///
/// On invalid input this returns a Result so the caller can log a safe,
/// non-sensitive error reason (never the raw payload).
fn sign_tx_result(tx_b64: &str, kp: &Keypair) -> Result<String, String> {
    let tx_bytes = general_purpose::STANDARD
        .decode(tx_b64)
        .map_err(|_| "invalid base64 in tx field".to_string())?;
    let tx: Transaction =
        bincode::deserialize(&tx_bytes).map_err(|_| "invalid bincode transaction".to_string())?;
    let msg = tx.message.serialize();
    let sig = kp.sign_message(&msg);
    Ok(general_purpose::STANDARD.encode(sig.as_ref()))
}

/// Build a structured audit-log line. Keeps the existing schema and only adds
/// a safe `error` field for failed requests. Never contains private key or
/// wallet material.
fn build_audit_line(
    now_ms: u128,
    request_id: &str,
    signature: &str,
    status: &str,
    error: Option<&str>,
) -> String {
    let mut line = serde_json::json!({
        "timestamp": now_ms,
        "request_id": request_id,
        "tx_hash": signature,
        "status": status,
        // Backward-compatible fields (kept for existing tooling)
        "ts_ms": now_ms,
        "signature_b64": signature,
    });
    if let Some(e) = error {
        line["error"] = serde_json::json!(e);
    }
    line.to_string()
}

/// Minimal CLI arg parser: `--certs <dir>` and `--log-file <path>`.
fn parse_args() -> (PathBuf, Option<PathBuf>) {
    let mut certs_dir = PathBuf::from("certs");
    let mut log_file: Option<PathBuf> = None;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--certs" => {
                i += 1;
                if i < args.len() {
                    certs_dir = PathBuf::from(&args[i]);
                }
            }
            "--log-file" => {
                i += 1;
                if i < args.len() {
                    log_file = Some(PathBuf::from(&args[i]));
                }
            }
            _ => {}
        }
        i += 1;
    }
    (certs_dir, log_file)
}

#[tokio::main]
async fn main() {
    let (certs_dir, log_file) = parse_args();
    let signer = Arc::new(load_or_create_signer());
    let audit = std::sync::Arc::new(AuditLogger::new(log_file));

    let audit2 = audit.clone();
    let signer2 = signer.clone();
    let sign = warp::post()
        .and(warp::path("sign"))
        .and(warp::body::json())
        .map(move |req: SignRequest| {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let fallback = general_purpose::STANDARD.encode([0u8; 64]);
            let (signature, status, error) = match sign_tx_result(&req.tx, &signer2) {
                Ok(sig) => (sig, "signed", None),
                Err(e) => (fallback, "failed", Some(e)),
            };
            let line =
                build_audit_line(now_ms, &request_id(), &signature, status, error.as_deref());
            audit2.log(&line.to_string());
            warp::reply::json(&SignResponse { signature })
        });

    // Public-key endpoint: lets the client build transactions without ever
    // loading a local keyfile when the remote HSM is the signing backend.
    let signer3 = signer.clone();
    let pubkey_route = warp::path("pubkey").and(warp::get()).map(move || {
        let pk = signer3.pubkey().to_string();
        warp::reply::json(&serde_json::json!({ "pubkey": pk }))
    });

    let routes = sign.or(pubkey_route).with(warp::log("hsm_server"));

    let cert_path = certs_dir.join("server.pem");
    let key_path = certs_dir.join("server.key");
    let ca_path = certs_dir.join("ca.pem");

    if cert_path.exists() && key_path.exists() && ca_path.exists() {
        println!(
            "[hsm_server] mTLS enabled: serving HTTPS on 127.0.0.1:8443 (client cert required)"
        );
        println!("[hsm_server] certs: {}", certs_dir.display());
        warp::serve(routes)
            .tls()
            .cert_path(cert_path)
            .key_path(key_path)
            .client_auth_required_path(ca_path)
            .run(([127, 0, 0, 1], 8443))
            .await;
    } else {
        // Fail-closed: never serve plain HTTP. mTLS is mandatory.
        eprintln!(
            "[hsm_server] FATAL: mTLS certs not found at {:?} (need server.pem, server.key, ca.pem). Refusing to start in plain HTTP mode (fail-closed).",
            certs_dir
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid unsigned transaction serialized to base64 bincode.
    fn valid_tx_b64(kp: &Keypair) -> String {
        let from = kp.pubkey();
        let to = solana_sdk::pubkey::Pubkey::new_from_array([0u8; 32]);
        let ix = solana_sdk::system_instruction::transfer(&from, &to, 1_000);
        let msg = solana_sdk::message::Message::new(&[ix], Some(&from));
        let tx = solana_sdk::transaction::Transaction::new_unsigned(msg);
        let bytes = bincode::serialize(&tx).unwrap();
        general_purpose::STANDARD.encode(&bytes)
    }

    #[test]
    fn sign_tx_result_ok_returns_64_byte_base64() {
        let kp = Keypair::new();
        let sig_b64 = sign_tx_result(&valid_tx_b64(&kp), &kp).unwrap();
        let decoded = general_purpose::STANDARD.decode(sig_b64).unwrap();
        assert_eq!(decoded.len(), 64);
    }

    #[test]
    fn sign_tx_result_reports_invalid_base64() {
        let kp = Keypair::new();
        let err = sign_tx_result("!!!not-base64!!!", &kp).unwrap_err();
        assert!(err.contains("base64"));
    }

    #[test]
    fn sign_tx_result_reports_invalid_bincode() {
        let kp = Keypair::new();
        let input = general_purpose::STANDARD.encode(vec![0u8; 32]);
        let err = sign_tx_result(&input, &kp).unwrap_err();
        assert!(err.contains("bincode"));
    }

    #[test]
    fn request_id_unique() {
        assert_ne!(request_id(), request_id());
    }

    #[test]
    fn audit_line_has_required_fields() {
        let line = build_audit_line(123, "req-1", "sig", "signed", None);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["request_id"], "req-1");
        assert_eq!(v["status"], "signed");
        assert_eq!(v["timestamp"], 123);
        assert_eq!(v["ts_ms"], 123);
        assert_eq!(v["tx_hash"], "sig");
        assert_eq!(v["signature_b64"], "sig");
        assert!(v.get("error").is_none());
    }

    #[test]
    fn audit_line_failed_includes_safe_error_only() {
        let line = build_audit_line(
            123,
            "req-2",
            "fallback",
            "failed",
            Some("invalid base64 in tx field"),
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error"], "invalid base64 in tx field");
        // No private key / wallet material may ever appear in the audit line.
        let s = line.to_lowercase();
        assert!(!s.contains("private key"));
        assert!(!s.contains("wallet"));
    }
}
