//! # Jito MEV Bundle Backend
//!
//! Emirleri **Jito Block Engine** REST API'sine bundle olarak gönderir
//! (MEV-korumalı, atomik yürütme). Önceden imzalanmış (pre-signed) serialized
//! transaction bytes'ları bir [`TransactionStore`] içinde `client_order_id`
//! anahtarıyla saklar.
//!
//! ## Neden pre-signed transaction store?
//! `ExecutionBackend::submit` senkron ve saf tutulur (üst katmandaki
//! retry/fallback/circuit mantığını deterministik kılmak için). İmzalama ve
//! serileştirme, gönderimden önce ayrı bir aşamada yapılır; sonuçta oluşan
//! ham byte'lar `client_order_id` ile store'a yazılır. `submit()` yalnızca bu
//! byte'ları alıp ağa iletir.
//!
//! ## Feature Flag
//! Gerçek HTTP bağımlılıkları (`reqwest`, `tokio`, `base64`) yalnızca `live`
//! feature'ı etkinken derlenir. Feature kapalıyken yapılar ve `TransactionStore`
//! yine kullanılabilir; `submit()` ise ağ katmanının devre dışı olduğunu
//! bildiren bir `Permanent` sonuç döndürür. Böylece varsayılan `cargo check`
//! ağır bağımlılıklar olmadan hatasız geçer.

use crate::backend::{ExecutionBackend, SubmitResult};
use crate::order::{ExecutionRoute, Order};
use crate::TransactionStore;

/// Mainnet Jito Block Engine bundle endpoint'i (varsayılan).
pub const DEFAULT_JITO_ENDPOINT: &str = "https://mainnet.block-engine.jito.labs";

/// Jito Block Engine bundle backend'i yapılandırması.
#[derive(Debug, Clone)]
pub struct JitoConfig {
    /// Block Engine temel URL'i.
    pub endpoint: String,
    /// Opsiyonel yetkilendirme token'ı.
    pub auth_token: Option<String>,
    /// HTTP isteği zaman aşımı (milisaniye).
    pub timeout_ms: u64,
}

impl Default for JitoConfig {
    fn default() -> Self {
        JitoConfig {
            endpoint: DEFAULT_JITO_ENDPOINT.to_string(),
            auth_token: None,
            timeout_ms: 5_000,
        }
    }
}

impl JitoConfig {
    /// Verilen endpoint ile yapılandırma oluşturur.
    pub fn new(endpoint: impl Into<String>) -> Self {
        JitoConfig {
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }

    /// Auth token ekler (builder tarzı).
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }
}

/// Jito bundle yürütme backend'i.
pub struct JitoBackend {
    // Only read by the `live` feature build (network submission).
    #[cfg_attr(not(feature = "live"), allow(dead_code))]
    config: JitoConfig,
    /// Emir kimliği → serialized tx deposu.
    store: TransactionStore,
    /// Canlı HTTP istemcisi (yalnızca `live` feature'ında).
    #[cfg(feature = "live")]
    client: reqwest::blocking::Client,
}

impl JitoBackend {
    /// Yeni bir Jito backend'i oluşturur.
    pub fn new(config: JitoConfig) -> Self {
        #[cfg(feature = "live")]
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(config.timeout_ms))
            .build()
            .expect("reqwest blocking client kurulamadı");

        JitoBackend {
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

    /// `sendBundle` JSON-RPC gövdesini oluşturur.
    // Only read by the `live` feature build (network submission).
    #[cfg_attr(not(feature = "live"), allow(dead_code))]
    fn build_bundle_body(tx_b64: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"sendBundle","params":[["{tx_b64}"]]}}"#)
    }
}

impl ExecutionBackend for JitoBackend {
    fn route(&self) -> ExecutionRoute {
        ExecutionRoute::JitoBundle
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

        self.submit_bundle(&raw)
    }
}

impl JitoBackend {
    /// Canlı destek DEVRE DIŞI: ağ yok, kalıcı hata döner.
    #[cfg(not(feature = "live"))]
    fn submit_bundle(&self, _raw: &[u8]) -> SubmitResult {
        tracing::warn!(target: "jito", "canlı Jito desteği derlenmedi (feature = live kapalı)");
        SubmitResult::Permanent {
            detail: "canlı Jito desteği devre dışı — crate'i `--features live` ile derleyin".into(),
        }
    }

    /// Canlı destek ETKİN: transaction'ı base64'e kodla ve Jito'ya gönder.
    #[cfg(feature = "live")]
    fn submit_bundle(&self, raw: &[u8]) -> SubmitResult {
        use base64::Engine;

        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let body = Self::build_bundle_body(&tx_b64);
        let url = format!(
            "{}/api/v1/bundles",
            self.config.endpoint.trim_end_matches('/')
        );

        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body);

        if let Some(token) = &self.config.auth_token {
            req = req.header("Authorization", token.clone());
        }

        tracing::info!(target: "jito", %url, "Jito bundle gönderiliyor");

        match req.send() {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                if status.is_success() {
                    match parse_rpc_result(&text) {
                        Some(bundle_id) => {
                            tracing::info!(target: "jito", %bundle_id, "bundle kabul edildi");
                            SubmitResult::Ok {
                                signature: bundle_id,
                            }
                        }
                        None => SubmitResult::Retryable {
                            detail: format!("Jito yanıtı ayrıştırılamadı: {text}"),
                        },
                    }
                } else if status.is_server_error() || status.as_u16() == 429 {
                    SubmitResult::Retryable {
                        detail: format!("Jito HTTP {status}: {text}"),
                    }
                } else {
                    SubmitResult::Permanent {
                        detail: format!("Jito HTTP {status}: {text}"),
                    }
                }
            }
            Err(e) if e.is_timeout() || e.is_connect() => SubmitResult::Retryable {
                detail: format!("Jito ağ hatası: {e}"),
            },
            Err(e) => SubmitResult::Permanent {
                detail: format!("Jito istek hatası: {e}"),
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
            route: ExecutionRoute::JitoBundle,
        }
    }

    #[test]
    fn jito_backend_route() {
        let b = JitoBackend::new(JitoConfig::default());
        assert_eq!(b.route(), ExecutionRoute::JitoBundle);
    }

    #[test]
    fn jito_kayitsiz_tx_permanent() {
        let mut b = JitoBackend::new(JitoConfig::default());
        assert!(matches!(
            b.submit(&order(1)),
            SubmitResult::Permanent { .. }
        ));
    }

    #[test]
    fn jito_bundle_body_formati() {
        let body = JitoBackend::build_bundle_body("AAAA");
        assert!(body.contains("\"method\":\"sendBundle\""));
        assert!(body.contains("[[\"AAAA\"]]"));
    }

    #[test]
    fn register_tx_sonrasi_store_dolu() {
        let mut b = JitoBackend::new(JitoConfig::new("https://x"));
        b.register_tx(5, vec![9, 9]);
        assert_eq!(b.store().get(5), Some(&[9, 9][..]));
    }
}
