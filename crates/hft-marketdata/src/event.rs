//! # Piyasa Olay Tipleri (Market Event Types)
//!
//! Normalize edilmiş piyasa olayları. Tüm kaynaklar (Geyser, replay, simülasyon)
//! bu ortak tipe dönüştürülür. `MarketEvent` tipi, kaynaktan bağımsız olarak
//! pipeline'da dolaşan temel veri birimidir.
//!
//! ## Tasarım
//! - `Copy` + küçük boyut: sıcak yolda heap tahsisi olmadan taşınabilir.
//! - `market_id` (u64): hangi piyasaya ait olduğunu belirtir.
//! - `slot` (u64): Solana slot numarası (sıralama ve dedup için).
//! - `seq` (u64): Kaynak bazında sıra numarası (boşluk tespiti için).
//! - `ingest_ts_ns` (u64): Pipeline'a giriş zamanı (latency ölçümü için).

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Olay türü — hangi tür güncellemenin geldiğini belirtir.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarketEventKind {
    /// Slot ilerlemesi (yeni slot duyurusu).
    SlotProgress { slot: u64 },
    /// Account güncellemesi (order book/gösterge değişikliği).
    AccountUpdate { slot: u64, pubkey: Vec<u8> },
    /// İşlem (transaction) güncellemesi.
    TransactionUpdate { slot: u64, signature: String },
    /// Emir defteri (order book) snapshot'ı.
    OrderBookSnapshot { slot: u64 },
    /// Gerçekleşmiş işlem (trade).
    Trade { slot: u64 },
    /// Sistem/gözetim olayı (bağlantı durumu, yeniden başlatma vb.).
    System { code: u32, message: String },
}

impl MarketEventKind {
    /// İlişkili slot numarasını döndürür (varsa).
    #[inline]
    pub fn slot(&self) -> Option<u64> {
        match self {
            MarketEventKind::SlotProgress { slot }
            | MarketEventKind::AccountUpdate { slot, .. }
            | MarketEventKind::TransactionUpdate { slot, .. }
            | MarketEventKind::OrderBookSnapshot { slot }
            | MarketEventKind::Trade { slot } => Some(*slot),
            MarketEventKind::System { .. } => None,
        }
    }
}

/// Pipeline'da dolaşan normalize edilmiş piyasa olayı.
///
/// # Örnek
/// ```
/// use hft_marketdata::event::{MarketEvent, MarketEventKind};
///
/// let event = MarketEvent::new(
///     1,          // market_id
///     100,        // slot
///     0,          // seq
///     900,        // publish_ts_ns
///     1000,       // ingest_ts_ns
///     MarketEventKind::SlotProgress { slot: 100 },
/// );
/// assert_eq!(event.market_id, 1);
/// ```
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketEvent {
    /// Piyasa/market tanımlayıcısı.
    pub market_id: u64,
    /// Solana slot numarası.
    pub slot: u64,
    /// Kaynak bazında sıra numarası (sequence).
    pub seq: u64,
    /// Olayın kaynakta oluşturulma zamanı (Unix nanosaniye).
    pub publish_ts_ns: u64,
    /// Pipeline'a giriş zamanı (Unix nanosaniye).
    pub ingest_ts_ns: u64,
    /// Olay türü.
    pub kind: MarketEventKind,
}

impl MarketEvent {
    /// Yeni bir piyasa olayı oluşturur.
    #[inline]
    pub fn new(
        market_id: u64,
        slot: u64,
        seq: u64,
        publish_ts_ns: u64,
        ingest_ts_ns: u64,
        kind: MarketEventKind,
    ) -> Self {
        MarketEvent {
            market_id,
            slot,
            seq,
            publish_ts_ns,
            ingest_ts_ns,
            kind,
        }
    }

    /// Pipeline'da geçen süre (end-to-end latency) nanosaniye cinsinden.
    #[inline]
    pub fn latency_ns(&self) -> u64 {
        self.ingest_ts_ns.saturating_sub(self.publish_ts_ns)
    }

    /// Olayın verili bir eşikten daha eski olup olmadığını kontrol eder.
    #[inline]
    pub fn is_stale(&self, max_age_ns: u64) -> bool {
        self.latency_ns() > max_age_ns
    }
}

/// Olayları slot bazında karşılaştırılabilir yapan wrapper.
/// Pipeline'da sıralama için kullanılır.
#[derive(Debug, Clone)]
pub struct OrderedEvent {
    /// İçteki olay.
    pub event: MarketEvent,
    /// Sıralama anahtarı (slot * K + seq gibi).
    pub order_key: u64,
}

impl OrderedEvent {
    /// Yeni bir sıralanabilir olay oluşturur.
    #[inline]
    pub fn new(event: MarketEvent, order_key: u64) -> Self {
        OrderedEvent { event, order_key }
    }
}

impl PartialEq for OrderedEvent {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.order_key == other.order_key
    }
}

impl Eq for OrderedEvent {}

impl PartialOrd for OrderedEvent {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedEvent {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.order_key.cmp(&other.order_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_olusturma_ve_latency() {
        let ev = MarketEvent::new(
            42,
            100,
            1,
            1_000,
            1_050,
            MarketEventKind::SlotProgress { slot: 100 },
        );
        assert_eq!(ev.market_id, 42);
        assert_eq!(ev.slot, 100);
        assert_eq!(ev.latency_ns(), 50);
        assert!(!ev.is_stale(100));
        assert!(ev.is_stale(30));
    }

    #[test]
    fn event_kind_slot() {
        let kind = MarketEventKind::SlotProgress { slot: 200 };
        assert_eq!(kind.slot(), Some(200));
        let sys = MarketEventKind::System {
            code: 0,
            message: "test".to_string(),
        };
        assert_eq!(sys.slot(), None);
    }

    #[test]
    fn ordered_event_siralama() {
        let e1 = OrderedEvent::new(
            MarketEvent::new(0, 1, 0, 0, 0, MarketEventKind::SlotProgress { slot: 1 }),
            100,
        );
        let e2 = OrderedEvent::new(
            MarketEvent::new(0, 2, 0, 0, 0, MarketEventKind::SlotProgress { slot: 2 }),
            200,
        );
        assert!(e1 < e2);
        assert!(e2 > e1);
    }
}
