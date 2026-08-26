//! # Piyasa Verisi Kaynağı Soyutlaması (Market Data Source)
//!
//! `MarketDataSource` trait'i, pipeline'ın beslendiği tüm kaynaklar için
//! ortak bir senkron `poll()` arayüzü tanımlar. Bu sayede:
//! - **Simülasyon/Replay**: Test ve geriye dönük test (backtest) için.
//! - **Canlı Geyser gRPC**: Yellowstone/Geyser akışı (async → sync köprüsü).
//!
//! ## Tasarım
//! - `poll()` senkron ve bloklamaz (`SourcePoll` ile).
//! - `SourcePoll::Idle` → veri yok, sonra tekrar dene.
//! - `SourcePoll::Event(e)` → yeni olay.
//! - `SourcePoll::Closed` → kaynak tükendi/kalıcı hata.

use crate::event::MarketEvent;

/// `MarketDataSource::poll()` dönüş tipleri.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcePoll {
    /// Kaynak hazır, henüz olay yok (tekrar dene).
    Idle,
    /// Yeni bir piyasa olayı.
    Event(MarketEvent),
    /// Kaynak kapandı veya kalıcı hata oluştu.
    Closed,
}

/// Piyasa verisi kaynağı için soyut arayüz.
///
/// Tüm kaynaklar (simülasyon, replay, canlı Geyser) bu trait'i
/// implemente eder. Pipeline, kaynağın türünden bağımsız olarak
/// `poll()` çağrısıyla veriyi tüketir.
pub trait MarketDataSource {
    /// Bir sonraki olayı poll et. Bloklamaz (non-blocking).
    ///
    /// # Dönüş
    /// - `SourcePoll::Idle`: Henüz olay yok.
    /// - `SourcePoll::Event(e)`: Yeni olay.
    /// - `SourcePoll::Closed`: Kaynak tükendi.
    fn poll(&mut self) -> SourcePoll;

    /// Kaynağın insan-okunur adı (loglama ve metrikler için).
    fn name(&self) -> &str;
}

/// Simüle edilmiş piyasa verisi kaynağı.
///
/// Önceden tanımlanmış bir olay listesini sırayla poll eder.
/// Test, benchmark ve replay senaryoları için kullanılır.
pub struct SimulatedSource {
    /// Kaynak adı.
    name: String,
    /// Kuyruktaki olaylar (ters çevrilmiş — pop_back için).
    events: Vec<MarketEvent>,
    /// Kaynağın kapanıp kapanmadığı.
    closed: bool,
}

impl SimulatedSource {
    /// Yeni bir simülasyon kaynağı oluşturur.
    ///
    /// # Örnek
    /// ```
    /// use hft_marketdata::event::{MarketEvent, MarketEventKind};
    /// use hft_marketdata::source::SimulatedSource;
    ///
    /// let events = vec![
    ///     MarketEvent::new(1, 100, 0, 0, 0, MarketEventKind::SlotProgress { slot: 100 }),
    /// ];
    /// let mut source = SimulatedSource::new("test", events);
    /// ```
    pub fn new(name: &str, events: Vec<MarketEvent>) -> Self {
        // Pop_back() ile sondan almak için ters çevir.
        let mut evts = events;
        evts.reverse();
        SimulatedSource {
            name: name.to_string(),
            events: evts,
            closed: false,
        }
    }

    /// Kalan olay sayısını döndürür.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.events.len()
    }

    /// Kaynağa yeni olaylar ekler (kuyruk sonuna).
    pub fn push(&mut self, event: MarketEvent) {
        self.events.insert(0, event);
    }
}

impl MarketDataSource for SimulatedSource {
    fn poll(&mut self) -> SourcePoll {
        if self.closed {
            return SourcePoll::Closed;
        }
        match self.events.pop() {
            Some(event) => SourcePoll::Event(event),
            None => {
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

/// Kaynaktan gelen ham olayları işlemek için yardımcı fonksiyonlar.
pub mod utils {
    use super::*;

    /// Bir kaynaktaki tüm olayları toplar (bloklayarak).
    /// Sadece test/benchmark senaryolarında kullanılmalıdır.
    pub fn drain_source(source: &mut dyn MarketDataSource) -> Vec<MarketEvent> {
        let mut events = Vec::new();
        loop {
            match source.poll() {
                SourcePoll::Event(e) => events.push(e),
                SourcePoll::Idle => continue,
                SourcePoll::Closed => break,
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MarketEventKind;

    #[test]
    fn simulated_source_tuketim() {
        let events = vec![
            MarketEvent::new(1, 10, 0, 0, 0, MarketEventKind::SlotProgress { slot: 10 }),
            MarketEvent::new(1, 11, 1, 0, 0, MarketEventKind::SlotProgress { slot: 11 }),
        ];
        let mut source = SimulatedSource::new("test", events);
        assert_eq!(source.name(), "test");

        match source.poll() {
            SourcePoll::Event(e) => assert_eq!(e.slot, 10),
            _ => panic!("ilk olay bekleniyordu"),
        }
        match source.poll() {
            SourcePoll::Event(e) => assert_eq!(e.slot, 11),
            _ => panic!("ikinci olay bekleniyordu"),
        }
        assert_eq!(source.poll(), SourcePoll::Closed);
    }

    #[test]
    fn simulated_source_push() {
        let mut source = SimulatedSource::new("push_test", vec![]);
        source.push(MarketEvent::new(
            1,
            99,
            0,
            0,
            0,
            MarketEventKind::SlotProgress { slot: 99 },
        ));
        match source.poll() {
            SourcePoll::Event(e) => assert_eq!(e.slot, 99),
            _ => panic!("push sonrası olay bekleniyordu"),
        }
    }

    #[test]
    fn drain_source_tum_olaylari_toplar() {
        let events = vec![
            MarketEvent::new(1, 1, 0, 0, 0, MarketEventKind::SlotProgress { slot: 1 }),
            MarketEvent::new(1, 2, 1, 0, 0, MarketEventKind::SlotProgress { slot: 2 }),
            MarketEvent::new(1, 3, 2, 0, 0, MarketEventKind::SlotProgress { slot: 3 }),
        ];
        let mut source = SimulatedSource::new("drain", events);
        let drained = utils::drain_source(&mut source);
        assert_eq!(drained.len(), 3);
    }
}
