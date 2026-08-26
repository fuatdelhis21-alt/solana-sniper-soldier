//! # Standart Solana RPC Backend (Fallback)
//!
//! Standart Solana RPC `sendTransaction` çağrısını yapar.
//! Jito bundle başarısız olduğunda yedek (fallback) yol olarak kullanılır.

use crate::backend::{ExecutionBackend, SubmitResult};
use crate::order::{ExecutionRoute, Order};
use crate::TransactionStore;

/// Standart Solana RPC backend'i yapılandırması.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// RPC endpoint URL'i.
    pub endpoint: String,
    /// Opsiyonel API anahtarı.
    pub api_key: Option<String>,
    /// HTTP isteği zaman aşımı (milisaniye).
    pub timeout_ms: u64,
    /// Pre-flight simülasyonunu atla.
    pub skip_preflight: bool,
}

impl Default for RpcConfig {
    fn default() -> Self {
        RpcConfig {
            endpoint: "https://api.mainnet-beta.solana.com".to_string(),
            api_key: None,
            timeout_ms: 5_000,
            skip_preflight: true,
        }
    }
}

impl RpcConfig {
    /// Verilen endpoint ile yapılandırma oluşturur.
    pub fn new(endpoint: impl Into<String>) -> Self {
        RpcConfig {
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }
}

/// Standart Solana RPC yürütme backend'i.
pub struct RpcBackend {
    config: RpcConfig,
    /// Emir kimliği → serialized tx deposu.
    store: TransactionStore,
    #[cfg(feature = "live")]
    client: reqwest::blocking::Client,
}

impl RpcBackend {
    /// Yeni bir RPC backend'i oluşturur.
    pub fn new(config: RpcConfig) -> Self {
        #[cfg(feature = "live")]
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(config.timeout_ms))
            .build()
            .expect("reqwest blocking client kurulamadı");

        RpcBackend {
            config,
            store: TransactionStore::new(),
            #[cfg(feature = "live")]
            client,
        }
    }

    /// Bir emir için önceden imzalanmış transaction bytes'ı kaydeder.
    pub fn register_tx(&mut self, client_order_id: u64, serialized_tx: Vec<u8>) {
        self.store.register(client_order_id, serialized_tx);
    }

    /// İç transaction deposuna salt-okunur erişim.
    pub fn store(&self) -> &TransactionStore {
        &self.store
    }

    /// İç transaction deposuna değiştirilebilir erişim.
    pub fn store_mut(&mut self) -> &mut TransactionStore {
        &mut self.store
    }

    /// `sendTransaction` JSON-RPC gövdesini oluşturur.
    fn build_send_tx_body(tx_b64: &str, skip_preflight: bool) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":["{tx_b64}",{{"encoding":"base64","skipPreflight":{skip_preflight},"maxRetries":0}}]}}"#
        )
    }
}

impl ExecutionBackend for RpcBackend {
    fn route(&self) -> ExecutionRoute {
        ExecutionRoute::Rpc
    }

    fn submit(&mut self, order: &Order) -> SubmitResult {
        let raw = match self.store.get(order.client_order_id) {
            Some(bytes) => bytes.to_vec(),
            None => {
                return SubmitResult::Permanent {
                    detail: format!(
                        "client_order_id={} için kayıtlı transaction yok (register_tx çağrılmadı)",
                        order.client_order_id
                    ),
                };
            }
        };

        self.submit_rpc(&raw)
    }
}

impl RpcBackend {
    /// Canlı destek DEVRE DIŞI: ağ yok, kalıcı hata döner.
    #[cfg(not(feature = "live"))]
    fn submit_rpc(&self, _raw: &[u8]) -> SubmitResult {
        tracing::warn!(target: "rpc", "canlı RPC desteği derlenmedi (feature = live kapalı)");
        SubmitResult::Permanent {
            detail: "canlı RPC desteği devre dışı — crate'i `--features live` ile derleyin".into(),
        }
    }

    /// Canlı destek ETKİN: transaction'ı RPC sendTransaction ile gönder.
    #[cfg(feature = "live")]
    fn submit_rpc(&self, raw: &[u8]) -> SubmitResult {
        use base64::Engine;

        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let body = Self::build_send_tx_body(&tx_b64, self.config.skip_preflight);

        let mut req = self
            .client
            .post(&self.config.endpoint)
            .header("Content-Type", "application/json")
            .body(body);

        if let Some(key) = &self.config.api_key {
            req = req.header("Authorization", key.clone());
        }

        tracing::info!(target: "rpc", endpoint = %self.config.endpoint, "RPC sendTransaction");

        match req.send() {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                if status.is_success() {
                    match parse_rpc_result(&text) {
                        Some(signature) => SubmitResult::Ok { signature },
                        None => SubmitResult::Retryable {
                            detail: format!("RPC yanıtı ayrıştırılamadı: {text}"),
                        },
                    }
                } else if status.is_server_error() || status.as_u16() == 429 {
                    SubmitResult::Retryable {
                        detail: format!("RPC HTTP {status}: {text}"),
                    }
                } else {
                    SubmitResult::Permanent {
                        detail: format!("RPC HTTP {status}: {text}"),
                    }
                }
            }
            Err(e) if e.is_timeout() || e.is_connect() => SubmitResult::Retryable {
                detail: format!("RPC ağ hatası: {e}"),
            },
            Err(e) => SubmitResult::Permanent {
                detail: format!("RPC istek hatası: {e}"),
            },
        }
    }
}

/// JSON-RPC yanıt gövdesinden `result` alanını ayıklar.
#[cfg(feature = "live")]
fn parse_rpc_result(body: &str) -> Option<String> {
    let key = "\"result\"";
    let idx = body.find(key)?;
    let rest = &body[idx + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let bytes = after.as_bytes();
    if bytes.first() == Some(&b'"') {
        let start = 1;
        let end = after[start..].find('"')? + start;
        Some(after[start..end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Order, Side};

    fn order(id: u64) -> Order {
        Order {
            client_order_id: id,
            market_id: 1,
            side: Side::Buy,
            quantity: 1,
            limit_price: 1_000_000_000,
            created_at_ns: 0,
            route: ExecutionRoute::Rpc,
        }
    }

    #[test]
    fn rpc_backend_route() {
        let b = RpcBackend::new(RpcConfig::default());
        assert_eq!(b.route(), ExecutionRoute::Rpc);
    }

    #[test]
    fn rpc_kayitsiz_tx_permanent() {
        let mut b = RpcBackend::new(RpcConfig::default());
        assert!(matches!(
            b.submit(&order(1)),
            SubmitResult::Permanent { .. }
        ));
    }

    #[test]
    fn rpc_send_tx_body_formati() {
        let body = RpcBackend::build_send_tx_body("AAAA", true);
        assert!(body.contains("\"method\":\"sendTransaction\""));
        assert!(body.contains("\"skipPreflight\":true"));
        assert!(body.contains("\"maxRetries\":0"));
    }

    #[test]
    fn register_tx_sonrasi_store_dolu() {
        let mut b = RpcBackend::new(RpcConfig::new("https://x"));
        b.register_tx(5, vec![9, 9]);
        assert_eq!(b.store().get(5), Some(&[9, 9][..]));
    }
}
