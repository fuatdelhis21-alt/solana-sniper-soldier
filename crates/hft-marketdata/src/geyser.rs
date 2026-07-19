//! # Yellowstone/Geyser gRPC Canlı Adaptörü (Live Adapter)
//!
//! Bu modül, canlı bir Yellowstone/Geyser gRPC akışını platformun senkron
//! `MarketDataSource` arayüzüne bağlar. Tasarımın kalbi bir **async → sync
//! köprüsü**dür: arka planda bir tokio runtime içinde gRPC stream tüketilir,
//! normalize edilen olaylar sınırlı (bounded) bir kanala yazılır; sıcak yoldaki
//! (hot path) `poll()` ise bu kanaldan **bloklamadan** okur.
//!
//! ## Mimari
//! ```text
//!   ┌──────────────────────┐    bounded channel     ┌──────────────────┐
//!   │  tokio arka thread   │  ───────────────────▶  │  poll() (sync)   │
//!   │  gRPC stream + retry │   MarketEvent akışı     │  MarketDataSource│
//!   └──────────────────────┘                         └──────────────────┘
//! ```
//!
//! ## Özellikler
//! - **Üstel geri çekilme (exponential backoff)** ile otomatik yeniden bağlanma.
//! - **`x-token` authentication** (Triton/Helius gibi sağlayıcılar için).
//! - Belirli **market/account pubkey** filtreleme.
//! - **Backpressure:** kanal dolduğunda en eski olay atılır, en yeni korunur
//!   (HFT'de güncel veri, eski veriden değerlidir).
//! - **İstatistik sayaçları:** bağlantı, alınan olay, yeniden bağlanma sayısı.
//! - **Graceful shutdown:** `Drop` ve açık `close()` ile arka thread durdurulur.
//!
//! ## Feature Flag
//! Gerçek gRPC bağımlılıkları (`yellowstone-grpc-client`, `tonic`, `tokio`)
//! yalnızca `live` feature'ı etkinken derlenir. Feature kapalıyken modül yine
//! derlenir (yapılar ve köprü mevcuttur) ancak `connect()` çağrısı, canlı
//! desteğin devre dışı olduğunu bildiren bir hata döndürür. Böylece varsayılan
//! `cargo check` ağır bağımlılıklar olmadan hatasız geçer.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam::channel::{bounded, Receiver, Sender, TryRecvError};

use crate::event::MarketEvent;
use crate::source::{MarketDataSource, SourcePoll};

/// Geyser adaptörüne özgü hata türleri.
#[derive(Debug, Clone)]
pub enum GeyserError {
    /// Bağlantı kurulamadı (ağ/TLS/handshake hatası).
    Connect(String),
    /// Abonelik (subscribe) kurulamadı.
    Subscribe(String),
    /// Akış (stream) sırasında hata oluştu.
    Stream(String),
    /// Yapılandırma geçersiz (ör. boş endpoint).
    Config(String),
    /// Canlı destek (`live` feature) derlenmemiş.
    FeatureDisabled,
}

impl std::fmt::Display for GeyserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeyserError::Connect(m) => write!(f, "geyser bağlantı hatası: {m}"),
            GeyserError::Subscribe(m) => write!(f, "geyser abonelik hatası: {m}"),
            GeyserError::Stream(m) => write!(f, "geyser akış hatası: {m}"),
            GeyserError::Config(m) => write!(f, "geyser yapılandırma hatası: {m}"),
            GeyserError::FeatureDisabled => write!(
                f,
                "canlı Geyser desteği devre dışı — crate'i `--features live` ile derleyin"
            ),
        }
    }
}

impl std::error::Error for GeyserError {}

/// Geyser bağlantı ve abonelik yapılandırması.
#[derive(Debug, Clone)]
pub struct GeyserConfig {
    /// gRPC endpoint URL'i (ör. `https://your-geyser:443`).
    pub endpoint: String,
    /// Opsiyonel `x-token` kimlik doğrulama başlığı (Triton/Helius).
    pub x_token: Option<String>,
    /// Filtrelenecek market/account pubkey'leri (base58). Boşsa tümü.
    pub account_filters: Vec<String>,
    /// Slot güncellemelerine abone ol.
    pub subscribe_slots: bool,
    /// İşlem (transaction) güncellemelerine abone ol.
    pub subscribe_transactions: bool,
    /// Yeniden bağlanmada başlangıç bekleme süresi (ms).
    pub backoff_initial_ms: u64,
    /// Üstel geri çekilme üst sınırı (ms).
    pub backoff_max_ms: u64,
    /// İç olay kanalının kapasitesi (backpressure tamponu).
    pub channel_capacity: usize,
}

impl Default for GeyserConfig {
    fn default() -> Self {
        GeyserConfig {
            endpoint: String::new(),
            x_token: None,
            account_filters: Vec::new(),
            subscribe_slots: true,
            subscribe_transactions: false,
            backoff_initial_ms: 250,
            backoff_max_ms: 10_000,
            channel_capacity: 65_536,
        }
    }
}

impl GeyserConfig {
    /// Verilen endpoint ile temel bir yapılandırma oluşturur.
    pub fn new(endpoint: impl Into<String>) -> Self {
        GeyserConfig {
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }

    /// `x-token` kimlik doğrulama başlığını ekler (builder tarzı).
    pub fn with_x_token(mut self, token: impl Into<String>) -> Self {
        self.x_token = Some(token.into());
        self
    }

    /// Filtrelenecek account pubkey'lerini ekler (builder tarzı).
    pub fn with_account_filters(mut self, accounts: Vec<String>) -> Self {
        self.account_filters = accounts;
        self
    }

    /// Yapılandırmayı doğrular; endpoint boş olamaz.
    fn validate(&self) -> Result<(), GeyserError> {
        if self.endpoint.trim().is_empty() {
            return Err(GeyserError::Config("endpoint boş olamaz".into()));
        }
        if self.channel_capacity == 0 {
            return Err(GeyserError::Config("channel_capacity sıfır olamaz".into()));
        }
        Ok(())
    }
}

/// Adaptörün çalışma zamanı istatistikleri (atomik — thread-safe okuma).
///
/// Arka thread bu sayaçları günceller; `poll()` tarafı ve gözlemleme kodu
/// kilit olmadan okuyabilir.
#[derive(Debug, Default)]
pub struct GeyserStats {
    /// Kurulan toplam bağlantı sayısı.
    pub connections: AtomicU64,
    /// Kanala başarıyla iletilen olay sayısı.
    pub events_received: AtomicU64,
    /// Yeniden bağlanma (reconnect) sayısı.
    pub reconnects: AtomicU64,
    /// Backpressure nedeniyle atılan (drop) olay sayısı.
    pub events_dropped: AtomicU64,
}

impl GeyserStats {
    /// İstatistiklerin anlık bir kopyasını (snapshot) döndürür.
    pub fn snapshot(&self) -> GeyserStatsSnapshot {
        GeyserStatsSnapshot {
            connections: self.connections.load(Ordering::Relaxed),
            events_received: self.events_received.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            events_dropped: self.events_dropped.load(Ordering::Relaxed),
        }
    }
}

/// `GeyserStats`'ın kopyalanabilir (plain) anlık görüntüsü.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeyserStatsSnapshot {
    /// Kurulan toplam bağlantı sayısı.
    pub connections: u64,
    /// Alınan toplam olay sayısı.
    pub events_received: u64,
    /// Yeniden bağlanma sayısı.
    pub reconnects: u64,
    /// Atılan olay sayısı.
    pub events_dropped: u64,
}

/// Canlı Geyser piyasa verisi kaynağı.
///
/// `MarketDataSource` trait'ini implemente eder. İç yapıda bir arka tokio
/// thread'i gRPC akışını tüketir; `poll()` ise sınırlı bir kanaldan bloklamadan
/// okur. Böylece pipeline'ın senkron/deterministik yapısı korunurken canlı
/// veri akışı sağlanır.
pub struct GeyserSource {
    /// İnsan-okunur kaynak adı (loglama için).
    name: String,
    /// Arka thread'den olayları alan kanal ucu.
    rx: Receiver<Result<MarketEvent, GeyserError>>,
    /// Paylaşılan istatistik sayaçları.
    stats: Arc<GeyserStats>,
    /// Graceful shutdown bayrağı — arka thread bunu izler.
    shutdown: Arc<AtomicBool>,
    /// Arka thread tutamacı (join için, Drop'ta beklenir).
    worker: Option<std::thread::JoinHandle<()>>,
    /// Kaynağın kapandığını (kanal koptu) hatırlar; sonraki poll'lar Closed döner.
    closed: bool,
}

impl GeyserSource {
    /// Yapılandırmayı doğrular, arka thread'i başlatır ve kaynağı döndürür.
    ///
    /// Arka thread bir tokio runtime kurar, endpoint'e bağlanır, abone olur ve
    /// gelen olayları normalize ederek kanala yazar. Bağlantı koparsa üstel
    /// geri çekilme ile yeniden dener.
    ///
    /// `live` feature'ı kapalıyken bu fonksiyon derlenir ancak arka thread,
    /// kanala tek bir `GeyserError::FeatureDisabled` yazıp sonlanır; `poll()`
    /// bu hatayı görür ve kaynağı kapatır.
    pub fn connect(config: GeyserConfig) -> Result<Self, GeyserError> {
        config.validate()?;

        let name = format!("geyser@{}", config.endpoint);
        let stats = Arc::new(GeyserStats::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        // Sınırlı (bounded) kanal: backpressure için kapasite sınırı.
        let (tx, rx) = bounded::<Result<MarketEvent, GeyserError>>(config.channel_capacity);

        // Arka thread — kendi tokio runtime'ını kurar (async → sync köprüsü).
        let worker_stats = Arc::clone(&stats);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_tx = tx.clone();
        // Drop-oldest backpressure için üreticiye bir okuyucu klonu verilir.
        let drain_rx = rx.clone();
        let cfg = config.clone();

        let worker = std::thread::Builder::new()
            .name("geyser-worker".to_string())
            .spawn(move || {
                run_worker(cfg, worker_tx, drain_rx, worker_stats, worker_shutdown);
            })
            .map_err(|e| GeyserError::Connect(format!("arka thread başlatılamadı: {e}")))?;

        tracing::info!(target: "geyser", endpoint = %config.endpoint, "Geyser kaynağı başlatıldı");

        Ok(GeyserSource {
            name,
            rx,
            stats,
            shutdown,
            worker: Some(worker),
            closed: false,
        })
    }

    /// İstatistiklerin anlık kopyasını döndürür.
    pub fn stats(&self) -> GeyserStatsSnapshot {
        self.stats.snapshot()
    }

    /// Arka thread'e durma sinyali gönderir ve tamamlanmasını bekler.
    /// Birden fazla çağrı güvenlidir (idempotent).
    pub fn close(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            // Arka thread bloklanmış olabilir; join en fazla thread sonlanana
            // kadar bekler. Kanal drop edildiğinde stream tarafı da uyanır.
            let _ = handle.join();
            tracing::info!(target: "geyser", "Geyser arka thread'i durduruldu");
        }
    }
}

impl MarketDataSource for GeyserSource {
    #[inline]
    fn poll(&mut self) -> SourcePoll {
        if self.closed {
            return SourcePoll::Closed;
        }
        match self.rx.try_recv() {
            Ok(Ok(event)) => SourcePoll::Event(event),
            Ok(Err(err)) => {
                // Arka thread kalıcı bir hata bildirdi — kaynağı kapat.
                tracing::error!(target: "geyser", error = %err, "Geyser kaynağı hata bildirdi");
                self.closed = true;
                SourcePoll::Closed
            }
            Err(TryRecvError::Empty) => SourcePoll::Idle,
            Err(TryRecvError::Disconnected) => {
                // Üretici sonlandı ve kanal boşaldı.
                tracing::warn!(target: "geyser", "Geyser kanalı koptu (disconnected)");
                self.closed = true;
                SourcePoll::Closed
            }
        }
    }

    #[inline]
    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for GeyserSource {
    fn drop(&mut self) {
        // Graceful shutdown — sızıntı (thread leak) olmaması için.
        self.close();
    }
}

/// Kanala backpressure'lı gönderim yardımcı fonksiyonu.
///
/// Kanal doluysa en eski olay `drain_rx` üzerinden atılır (drop-oldest) ve yeni
/// olay yeniden denenir. HFT'de en güncel veri en değerlidir; bu nedenle eski
/// veri feda edilir.
#[allow(dead_code)]
fn send_with_backpressure(
    tx: &Sender<Result<MarketEvent, GeyserError>>,
    drain_rx: &Receiver<Result<MarketEvent, GeyserError>>,
    stats: &GeyserStats,
    item: Result<MarketEvent, GeyserError>,
) {
    use crossbeam::channel::TrySendError;
    let mut payload = item;
    loop {
        match tx.try_send(payload) {
            Ok(()) => {
                stats.events_received.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(TrySendError::Full(returned)) => {
                // En eski olayı at, sonra tekrar dene.
                if drain_rx.try_recv().is_ok() {
                    stats.events_dropped.fetch_add(1, Ordering::Relaxed);
                }
                payload = returned;
            }
            Err(TrySendError::Disconnected(_)) => {
                // Tüketici gitti — göndermenin anlamı yok.
                return;
            }
        }
    }
}

// =============================================================================
// Arka thread implementasyonu — `live` feature'ına göre değişir.
// =============================================================================

/// Canlı destek DEVRE DIŞI: arka thread yalnızca bir hata yazıp sonlanır.
#[cfg(not(feature = "live"))]
fn run_worker(
    _config: GeyserConfig,
    tx: Sender<Result<MarketEvent, GeyserError>>,
    _drain_rx: Receiver<Result<MarketEvent, GeyserError>>,
    _stats: Arc<GeyserStats>,
    _shutdown: Arc<AtomicBool>,
) {
    tracing::warn!(
        target: "geyser",
        "canlı Geyser desteği derlenmedi (feature = live kapalı)"
    );
    // Tek seferlik hata; poll() bunu görüp kaynağı kapatır.
    let _ = tx.send(Err(GeyserError::FeatureDisabled));
}

/// Canlı destek ETKİN: tokio runtime kur, bağlan, abone ol, olayları köprüle.
///
/// Bağlantı koptuğunda üstel geri çekilme (exponential backoff) ile yeniden
/// dener. Shutdown bayrağı set edildiğinde döngü sonlanır.
#[cfg(feature = "live")]
fn run_worker(
    config: GeyserConfig,
    tx: Sender<Result<MarketEvent, GeyserError>>,
    drain_rx: Receiver<Result<MarketEvent, GeyserError>>,
    stats: Arc<GeyserStats>,
    shutdown: Arc<AtomicBool>,
) {
    // Arka thread'e özel çok-thread'li tokio runtime.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = tx.send(Err(GeyserError::Connect(format!(
                "tokio runtime kurulamadı: {e}"
            ))));
            return;
        }
    };

    runtime.block_on(async move {
        let mut backoff_ms = config.backoff_initial_ms;

        while !shutdown.load(Ordering::SeqCst) {
            match live::connect_and_stream(&config, &tx, &drain_rx, &stats, &shutdown).await {
                Ok(()) => {
                    // Stream düzgün kapandı — yeniden bağlanmayı dene.
                    tracing::warn!(target: "geyser", "Geyser akışı kapandı, yeniden bağlanılacak");
                }
                Err(err) => {
                    tracing::error!(target: "geyser", error = %err, "Geyser akış hatası");
                }
            }

            if shutdown.load(Ordering::SeqCst) {
                break;
            }

            // Üstel geri çekilme ile bekle.
            stats.reconnects.fetch_add(1, Ordering::Relaxed);
            tracing::info!(target: "geyser", backoff_ms, "yeniden bağlanma öncesi bekleniyor");
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(config.backoff_max_ms);
        }

        tracing::info!(target: "geyser", "Geyser worker döngüsü sonlandı");
    });
}

/// Canlı gRPC bağlantı mantığı — yalnızca `live` feature'ında derlenir.
#[cfg(feature = "live")]
mod live {
    use super::*;
    use yellowstone_grpc_client::GeyserGrpcClient;
    use yellowstone_grpc_proto::prelude::{
        subscribe_update::UpdateOneof, SubscribeRequest, SubscribeRequestFilterAccounts,
        SubscribeRequestFilterSlots, SubscribeRequestFilterTransactions,
    };

    /// Tek bir bağlantı oturumu: bağlan, abone ol, akışı tüketip köprüle.
    /// Bağlantı koparsa `Err`, düzgün kapanırsa `Ok(())` döner.
    pub(super) async fn connect_and_stream(
        config: &GeyserConfig,
        tx: &Sender<Result<MarketEvent, GeyserError>>,
        drain_rx: &Receiver<Result<MarketEvent, GeyserError>>,
        stats: &GeyserStats,
        shutdown: &Arc<AtomicBool>,
    ) -> Result<(), GeyserError> {
        use futures::StreamExt;
        use std::collections::HashMap;

        // İstemciyi kur (x-token varsa ekle).
        let mut builder = GeyserGrpcClient::build_from_shared(config.endpoint.clone())
            .map_err(|e| GeyserError::Connect(e.to_string()))?;
        if let Some(token) = &config.x_token {
            builder = builder
                .x_token(Some(token.clone()))
                .map_err(|e| GeyserError::Connect(e.to_string()))?;
        }
        let mut client = builder
            .connect()
            .await
            .map_err(|e| GeyserError::Connect(e.to_string()))?;

        stats.connections.fetch_add(1, Ordering::Relaxed);
        tracing::info!(target: "geyser", endpoint = %config.endpoint, "Geyser'e bağlanıldı");

        // Abonelik isteği: slot / account / transaction filtreleri.
        let mut accounts = HashMap::new();
        if !config.account_filters.is_empty() {
            accounts.insert(
                "hft-accounts".to_string(),
                SubscribeRequestFilterAccounts {
                    account: config.account_filters.clone(),
                    owner: vec![],
                    filters: vec![],
                    ..Default::default()
                },
            );
        }

        let mut slots = HashMap::new();
        if config.subscribe_slots {
            slots.insert(
                "hft-slots".to_string(),
                SubscribeRequestFilterSlots::default(),
            );
        }

        let mut transactions = HashMap::new();
        if config.subscribe_transactions {
            transactions.insert(
                "hft-tx".to_string(),
                SubscribeRequestFilterTransactions {
                    account_include: config.account_filters.clone(),
                    ..Default::default()
                },
            );
        }

        let request = SubscribeRequest {
            accounts,
            slots,
            transactions,
            ..Default::default()
        };

        let (_sink, mut stream) = client
            .subscribe_with_request(Some(request))
            .await
            .map_err(|e| GeyserError::Subscribe(e.to_string()))?;

        // Akış döngüsü — her mesajı normalize edip kanala köprüle.
        let mut sequence: u64 = 0;
        while let Some(message) = stream.next().await {
            if shutdown.load(Ordering::SeqCst) {
                tracing::info!(target: "geyser", "shutdown sinyali — akış sonlandırılıyor");
                return Ok(());
            }

            let update = message.map_err(|e| GeyserError::Stream(e.to_string()))?;
            let now_ns = super::now_unix_nanos();

            if let Some(oneof) = update.update_oneof {
                if let Some(event) = map_update(oneof, &mut sequence, now_ns) {
                    super::send_with_backpressure(tx, drain_rx, stats, Ok(event));
                }
            }
        }

        Ok(())
    }

    /// Yellowstone `UpdateOneof` varyantını platformun `MarketEvent`'ine çevirir.
    /// Desteklenmeyen/ilgisiz güncellemeler için `None` döner.
    fn map_update(
        oneof: UpdateOneof,
        sequence: &mut u64,
        now_ns: u64,
    ) -> Option<MarketEvent> {
        use crate::event::MarketEventKind;

        match oneof {
            // Slot ilerlemesi → SlotProgress olayı.
            UpdateOneof::Slot(slot_update) => {
                *sequence += 1;
                Some(MarketEvent::new(
                    0,
                    slot_update.slot,
                    *sequence,
                    now_ns,
                    now_ns,
                    MarketEventKind::SlotProgress {
                        slot: slot_update.slot,
                    },
                ))
            }
            // Account güncellemesi → şu an SlotProgress olarak işaretlenir.
            // (Gerçek order book decode'u program-özel layout gerektirir; burada
            //  slot ilerlemesi olarak köprülenir. Decode mantığı ayrı bir aşama.)
            UpdateOneof::Account(account_update) => {
                *sequence += 1;
                let slot = account_update.slot;
                // market_id: account pubkey'inin ilk 8 baytından türetilir.
                let market_id = account_update
                    .account
                    .as_ref()
                    .map(|a| pubkey_to_market_id(&a.pubkey))
                    .unwrap_or(0);
                Some(MarketEvent::new(
                    market_id,
                    slot,
                    *sequence,
                    now_ns,
                    now_ns,
                    MarketEventKind::SlotProgress { slot },
                ))
            }
            // Transaction güncellemesi → slot ilerlemesi olarak köprülenir.
            UpdateOneof::Transaction(tx_update) => {
                *sequence += 1;
                let slot = tx_update.slot;
                Some(MarketEvent::new(
                    0,
                    slot,
                    *sequence,
                    now_ns,
                    now_ns,
                    MarketEventKind::SlotProgress { slot },
                ))
            }
            // Ping/pong ve diğer kontrol mesajları yok sayılır.
            _ => None,
        }
    }

    /// Pubkey byte'larından deterministik bir `market_id` (u64) türetir.
    fn pubkey_to_market_id(pubkey: &[u8]) -> u64 {
        let mut buf = [0u8; 8];
        let n = pubkey.len().min(8);
        buf[..n].copy_from_slice(&pubkey[..n]);
        u64::from_le_bytes(buf)
    }
}

/// Geçerli Unix zamanını nanosaniye olarak döndürür (monotonik değil; kaynak
/// zaman damgası olarak kullanılır).
#[inline]
#[allow(dead_code)] // yalnızca `live` feature'ında kullanılır
fn now_unix_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gecersiz_config_reddedilir() {
        let cfg = GeyserConfig::new("");
        assert!(matches!(
            GeyserConfig::validate(&cfg),
            Err(GeyserError::Config(_))
        ));
    }

    #[test]
    fn builder_x_token_ekler() {
        let cfg = GeyserConfig::new("https://x:443").with_x_token("tok");
        assert_eq!(cfg.x_token.as_deref(), Some("tok"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn backpressure_en_eskiyi_atar() {
        // Kapasite 1 olan bir kanal: ikinci gönderim, ilkini atmalı.
        let (tx, rx) = bounded::<Result<MarketEvent, GeyserError>>(1);
        let stats = GeyserStats::default();

        let ev = |slot: u64| {
            Ok(MarketEvent::new(
                0,
                slot,
                slot,
                0,
                0,
                crate::event::MarketEventKind::SlotProgress { slot },
            ))
        };

        send_with_backpressure(&tx, &rx, &stats, ev(1));
        send_with_backpressure(&tx, &rx, &stats, ev(2));

        // Kanalda tek olay kalmalı ve o en yeni (slot=2) olmalı.
        let got = rx.try_recv().unwrap().unwrap();
        assert_eq!(got.slot, 2);
        assert_eq!(stats.events_dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn feature_disabled_worker_kaynagi_kapatir() {
        // live feature kapalıyken connect() sonrası poll() Closed dönmeli.
        #[cfg(not(feature = "live"))]
        {
            let mut src = GeyserSource::connect(GeyserConfig::new("https://x:443")).unwrap();
            // Arka thread hata yazana kadar kısa bekle.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut saw_closed = false;
            for _ in 0..10 {
                match src.poll() {
                    SourcePoll::Closed => {
                        saw_closed = true;
                        break;
                    }
                    _ => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            assert!(saw_closed);
        }
    }
}
