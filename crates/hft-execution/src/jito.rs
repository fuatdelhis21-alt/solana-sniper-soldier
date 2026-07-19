//! # Jito MEV Bundle + RPC Fallback Adaptörü (Canlı Backend'ler)
//!
//! Bu modül, `ExecutionBackend` trait'ini gerçek ağ yolları için implemente
//! eden iki backend sağlar:
//!
//! - [`JitoBackend`]: Emirleri **Jito Block Engine** REST API'sine bundle
//!   olarak gönderir (MEV-korumalı, atomik yürütme). Önceden imzalanmış
//!   (pre-signed) serialized transaction bytes'ları bir [`TransactionStore`]
//!   içinde `client_order_id` anahtarıyla saklar.
//! - [`RpcBackend`]: Standart Solana RPC `sendTransaction` çağrısını yapar
//!   (yedek/fallback yol). Pre-flight simülasyonu atlama seçeneği vardır.
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

use std::collections::HashMap;

use crate::backend::{ExecutionBackend, SubmitResult};
use crate::order::{ExecutionRoute, Order};

/// Mainnet Jito Block Engine bundle endpoint'i (varsayılan).
pub const DEFAULT_JITO_ENDPOINT: &str = "https://mainnet.block-engine.jito.labs";

/// Bir `client_order_id` → önceden imzalanmış serialized transaction bytes
/// eşlemesi tutan depo.
///
/// İmzalama/serileştirme aşaması (bu modülün dışında) sonuç byte'larını buraya
/// yazar; `submit()` çağrısında ilgili emir için byte'lar buradan okunur.
#[derive(Debug, Default, Clone)]
pub struct TransactionStore {
    /// client_order_id → serialized transaction (ham bytes).
    store: HashMap<u64, Vec<u8>>,
}

impl TransactionStore {
    /// Boş bir depo oluşturur.
    pub fn new() -> Self {
        TransactionStore {
            store: HashMap::new(),
        }
    }

    /// Bir emir için serialized transaction bytes kaydeder. Aynı kimlikle
    /// tekrar yazım, öncekini üzerine yazar (idempotent güncelleme).
    pub fn register(&mut self, client_order_id: u64, tx: Vec<u8>) {
        self.store.insert(client_order_id, tx);
    }

    /// Bir emrin kayıtlı transaction byte'larına salt-okunur erişim.
    pub fn get(&self, client_order_id: u64) -> Option<&[u8]> {
        self.store.get(&client_order_id).map(|v| v.as_slice())
    }

    /// Bir emrin kaydını depodan çıkarır ve sahipliğini döndürür.
    pub fn remove(&mut self, client_order_id: u64) -> Option<Vec<u8>> {
        self.store.remove(&client_order_id)
    }

    /// Depodaki kayıt sayısı.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Depo boş mu?
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

// =============================================================================
// Jito Backend
// =============================================================================

/// Jito Block Engine bundle backend'i yapılandırması.
#[derive(Debug, Clone)]
pub struct JitoConfig {
    /// Block Engine temel URL'i (ör. `https://mainnet.block-engine.jito.labs`).
    pub endpoint: String,
    /// Opsiyonel yetkilendirme (auth) token'ı — `Authorization` başlığına eklenir.
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
///
/// `ExecutionRoute::JitoBundle` yolunu temsil eder. `submit()` çağrısında,
/// emrin `client_order_id`'sine karşılık gelen önceden imzalanmış transaction'ı
/// `TransactionStore`'dan alır, base64'e kodlar ve Jito `sendBundle` API'sine
/// gönderir. Başarılıysa dönen `bundle_id`'yi imza olarak döndürür.
pub struct JitoBackend {
    #[allow(dead_code)] // yalnızca `live` feature'ında okunur
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
    /// `submit()` çağrılmadan önce ilgili emir için mutlaka çağrılmalıdır.
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

    /// `sendBundle` JSON-RPC gövdesini oluşturur (base64 tx listesi ile).
    #[allow(dead_code)] // `live` + testlerde kullanılır
    fn build_bundle_body(tx_b64: &str) -> String {
        // Jito sendBundle: params = [ [<base64_tx>, ...] ]
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"sendBundle","params":[["{tx_b64}"]]}}"#
        )
    }
}

impl ExecutionBackend for JitoBackend {
    fn route(&self) -> ExecutionRoute {
        ExecutionRoute::JitoBundle
    }

    fn submit(&mut self, order: &Order) -> SubmitResult {
        // Emir için kayıtlı transaction byte'larını al.
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
        let url = format!("{}/api/v1/bundles", self.config.endpoint.trim_end_matches('/'));

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
                    // Yanıttan bundle_id'yi ayıkla (result alanı).
                    match parse_rpc_result(&text) {
                        Some(bundle_id) => {
                            tracing::info!(target: "jito", %bundle_id, "bundle kabul edildi");
                            SubmitResult::Ok { signature: bundle_id }
                        }
                        None => SubmitResult::Retryable {
                            detail: format!("Jito yanıtı ayrıştırılamadı: {text}"),
                        },
                    }
                } else if status.is_server_error() || status.as_u16() == 429 {
                    // 5xx veya rate-limit → yeniden denenebilir.
                    SubmitResult::Retryable {
                        detail: format!("Jito HTTP {status}: {text}"),
                    }
                } else {
                    // 4xx (rate-limit hariç) → kalıcı hata.
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

// =============================================================================
// RPC Fallback Backend
// =============================================================================

/// Standart Solana RPC backend'i yapılandırması.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// RPC endpoint URL'i (ör. `https://api.mainnet-beta.solana.com`).
    pub endpoint: String,
    /// Opsiyonel API anahtarı — `Authorization` başlığına eklenir.
    pub api_key: Option<String>,
    /// HTTP isteği zaman aşımı (milisaniye).
    pub timeout_ms: u64,
    /// Pre-flight simülasyonunu atla (`skipPreflight`).
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

/// Standart Solana RPC yürütme backend'i (yedek/fallback yol).
///
/// `ExecutionRoute::Rpc` yolunu temsil eder. `submit()`, emrin kayıtlı
/// transaction'ını RPC `sendTransaction` metoduna gönderir. `skipPreflight`
/// ve `maxRetries: 0` (retry üst katmanda) ile en düşük gecikme hedeflenir.
pub struct RpcBackend {
    #[allow(dead_code)] // yalnızca `live` feature'ında okunur
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
    #[allow(dead_code)] // `live` + testlerde kullanılır
    fn build_send_tx_body(tx_b64: &str, skip_preflight: bool) -> String {
        // encoding=base64, skipPreflight, maxRetries=0 (retry üst katmanda).
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

/// JSON-RPC yanıt gövdesinden `result` alanını (string imza/bundle_id) ayıklar.
/// `serde_json` bağımlılığı olmadan basit ve dayanıklı bir ayrıştırma yapar.
#[cfg(feature = "live")]
fn parse_rpc_result(body: &str) -> Option<String> {
    // "result":"<değer>" desenini ara.
    let key = "\"result\"";
    let idx = body.find(key)?;
    let rest = &body[idx + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    // String değer bekleniyor: ilk tırnaktan ikinci tırnağa kadar.
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
    use hft_core::market::{Price, Quantity, Side};

    fn order(id: u64) -> Order {
        Order {
            client_order_id: id,
            market_id: 1,
            side: Side::Bid,
            quantity: Quantity::from_raw(1),
            limit_price: Price::from_f64(1.0),
            created_at_ns: 0,
        }
    }

    #[test]
    fn transaction_store_register_get_remove() {
        let mut store = TransactionStore::new();
        assert!(store.is_empty());
        store.register(7, vec![1, 2, 3]);
        assert_eq!(store.get(7), Some(&[1, 2, 3][..]));
        assert_eq!(store.len(), 1);
        assert_eq!(store.remove(7), Some(vec![1, 2, 3]));
        assert!(store.get(7).is_none());
    }

    #[test]
    fn jito_backend_route_ve_kayitsiz_tx_permanent() {
        let mut b = JitoBackend::new(JitoConfig::default());
        assert_eq!(b.route(), ExecutionRoute::JitoBundle);
        // Kayıtlı tx yokken submit → Permanent hata.
        assert!(matches!(b.submit(&order(1)), SubmitResult::Permanent { .. }));
    }

    #[test]
    fn rpc_backend_route_ve_kayitsiz_tx_permanent() {
        let mut b = RpcBackend::new(RpcConfig::default());
        assert_eq!(b.route(), ExecutionRoute::Rpc);
        assert!(matches!(b.submit(&order(1)), SubmitResult::Permanent { .. }));
    }

    #[test]
    fn jito_bundle_body_formati() {
        let body = JitoBackend::build_bundle_body("AAAA");
        assert!(body.contains("\"method\":\"sendBundle\""));
        assert!(body.contains("[[\"AAAA\"]]"));
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
        let mut b = JitoBackend::new(JitoConfig::new("https://x"));
        b.register_tx(5, vec![9, 9]);
        assert_eq!(b.store().get(5), Some(&[9, 9][..]));
    }
}
