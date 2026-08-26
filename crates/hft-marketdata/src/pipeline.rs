//! # Piyasa Verisi Boru Hattı (Market Data Pipeline)
//!
//! Birden fazla işlem aşamasını uçtan uca birleştiren ana boru hattı.
//! Sırasıyla: kaynak → dedup → ring buffer (slot sıralama) → latency izleme
//! → tüketici (callback).
//!
//! ## Akış
//! ```text
//! Kaynak (MarketDataSource)
//!   │
//!   ▼
//! Deduplicator ──→ (tekrarları at)
//!   │
//!   ▼
//! SlotRingBuffer ──→ (slot bazında sırala)
//!   │
//!   ▼
//! LatencyMonitor ──→ (bayat veriyi işaretle)
//!   │
//!   ▼
//! Tüketici (callback)
//! ```

use crate::dedup::Deduplicator;
use crate::event::MarketEvent;
use crate::latency::LatencyMonitor;
use crate::ring::SlotRingBuffer;
use crate::source::{MarketDataSource, SourcePoll};

/// Pipeline yapılandırması.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Slot ring buffer boyutu (güçlü sıralama için).
    pub ring_buffer_slots: usize,
    /// Bayat-veri eşiği (nanosaniye).
    pub stale_threshold_ns: u64,
    /// Maksimum market sayısı (dedup için).
    pub max_markets: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            ring_buffer_slots: 64,
            stale_threshold_ns: 1_000_000_000, // 1 saniye
            max_markets: 256,
        }
    }
}

/// Pipeline istatistikleri (anlık görüntü).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineStats {
    /// Toplam okunan olay sayısı.
    pub total_polled: u64,
    /// Dedup tarafından filtrelenen olay sayısı.
    pub dedup_filtered: u64,
    /// Ring buffer'dan salınan (released) olay sayısı.
    pub released: u64,
    /// Tüketiciye iletilen olay sayısı.
    pub consumed: u64,
    /// Bayat (stale) olarak işaretlenen olay sayısı.
    pub stale_count: u64,
    /// Anlık latency istatistikleri.
    pub latency: crate::latency::LatencySnapshot,
}

/// Uçtan uca piyasa verisi boru hattı.
///
/// Tüm aşamaları (dedup, sıralama, latency izleme) birleştirir ve
/// kaynaktan gelen olayları işleyerek tüketici callback'ine iletir.
///
/// # Örnek
/// ```
/// use hft_marketdata::pipeline::{MarketDataPipeline, PipelineConfig};
/// use hft_marketdata::event::{MarketEvent, MarketEventKind};
/// use hft_marketdata::source::SimulatedSource;
///
/// let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
/// let events = vec![
///     MarketEvent::new(1, 100, 0, 900, 1000, MarketEventKind::SlotProgress { slot: 100 }),
///     MarketEvent::new(1, 101, 0, 900, 1000, MarketEventKind::SlotProgress { slot: 101 }),
/// ];
/// let mut source = SimulatedSource::new("demo", events);
/// let released = pipeline.run_to_completion(&mut source, |e| e.ingest_ts_ns + 50);
/// assert_eq!(released.len(), 2);
/// ```
pub struct MarketDataPipeline {
    /// Yinelenen olay filtresi.
    dedup: Deduplicator,
    /// Slot sıralama tamponu.
    ring: SlotRingBuffer,
    /// Latency izleyici.
    latency: LatencyMonitor,
    /// Pipeline yapılandırması.
    config: PipelineConfig,
    /// Toplam poll sayısı.
    total_polled: u64,
    /// Tüketiciye iletilen olay sayısı.
    consumed: u64,
}

impl MarketDataPipeline {
    /// Yeni bir pipeline oluşturur.
    pub fn new(config: PipelineConfig) -> Self {
        MarketDataPipeline {
            dedup: Deduplicator::with_capacity(config.max_markets),
            ring: SlotRingBuffer::new(config.ring_buffer_slots),
            latency: LatencyMonitor::with_stale_threshold(config.stale_threshold_ns),
            config,
            total_polled: 0,
            consumed: 0,
        }
    }

    /// Kaynaktan bir sonraki olayı poll eder ve pipeline'dan geçirir.
    ///
    /// Dönüş:
    /// - `Some(event)`: Pipeline'dan geçmiş, tüketilmeye hazır olay.
    /// - `None`: Kaynak boş (Idle) veya kapandı (Closed).
    pub fn next(&mut self, source: &mut dyn MarketDataSource) -> Option<MarketEvent> {
        loop {
            match source.poll() {
                SourcePoll::Idle => return None,
                SourcePoll::Closed => return None,
                SourcePoll::Event(event) => {
                    self.total_polled += 1;

                    // 1. Dedup kontrolü.
                    if !self.dedup.is_new(&event) {
                        continue; // Tekrar → atla
                    }

                    // Watermark = event slot değeri (push event move edecek, önce oku).
                    let watermark = event.slot;

                    // 2. Ring buffer'a ekle (slot sıralama).
                    self.ring.push(event);

                    // 3. Ring buffer'dan sıralı olarak al.
                    while let Some(ordered) = self.ring.pop_ready(watermark) {
                        // 4. Latency izle.
                        self.latency.observe(&ordered);
                        self.consumed += 1;
                        return Some(ordered);
                    }
                }
            }
        }
    }

    /// Bir kaynağı sonuna kadar tüketir ve tüm olayları döndürür.
    /// Her olay için verilen `release_after_ns` fonksiyonu kullanılarak
    /// ring buffer'dan salınma zamanı belirlenir.
    ///
    /// Bu fonksiyon bloklayıcıdır; sadece test/benchmark amaçlıdır.
    pub fn run_to_completion(
        &mut self,
        source: &mut dyn MarketDataSource,
        _release_after_ns: impl Fn(&MarketEvent) -> u64,
    ) -> Vec<MarketEvent> {
        let mut result = Vec::new();
        loop {
            match source.poll() {
                SourcePoll::Idle => continue,
                SourcePoll::Closed => break,
                SourcePoll::Event(event) => {
                    self.total_polled += 1;

                    // Dedup.
                    if !self.dedup.is_new(&event) {
                        continue;
                    }

                    // Watermark = event slot değeri (push event move edecek, önce oku).
                    let watermark = event.slot;

                    // Ring buffer.
                    self.ring.push(event);

                    // Sıralı çıkış — watermark değerini kullan.
                    while let Some(ordered) = self.ring.pop_ready(watermark) {
                        self.latency.observe(&ordered);
                        self.consumed += 1;
                        result.push(ordered);
                    }
                }
            }
        }
        // Kalan olayları boşalt — drained ile tüm kalanları al.
        for event in self.ring.drain() {
            self.latency.observe(&event);
            self.consumed += 1;
            result.push(event);
        }
        result
    }

    /// Mevcut pipeline istatistiklerini döndürür.
    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            total_polled: self.total_polled,
            dedup_filtered: self.dedup.filtered_count(),
            released: self.ring.released_count(),
            consumed: self.consumed,
            stale_count: self.latency.snapshot().stale_count,
            latency: self.latency.snapshot(),
        }
    }

    /// Deduplicator'a salt-okunur erişim.
    pub fn dedup(&self) -> &Deduplicator {
        &self.dedup
    }

    /// Latency monitor'a salt-okunur erişim.
    pub fn latency(&self) -> &LatencyMonitor {
        &self.latency
    }

    /// Ring buffer'a salt-okunur erişim.
    pub fn ring(&self) -> &SlotRingBuffer {
        &self.ring
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MarketEventKind;
    use crate::source::SimulatedSource;

    #[test]
    fn pipeline_bos_kaynak() {
        let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
        let mut source = SimulatedSource::new("empty", vec![]);
        assert!(pipeline.next(&mut source).is_none());
    }

    #[test]
    fn pipeline_tek_olay() {
        let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
        let events = vec![MarketEvent::new(
            1,
            100,
            0,
            0,
            0,
            MarketEventKind::SlotProgress { slot: 100 },
        )];
        let mut source = SimulatedSource::new("single", events);
        let ev = pipeline.next(&mut source);
        assert!(ev.is_some());
        assert_eq!(ev.unwrap().slot, 100);
    }

    #[test]
    fn pipeline_dedup_calisir() {
        let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
        let ev = MarketEvent::new(1, 100, 0, 0, 0, MarketEventKind::SlotProgress { slot: 100 });
        let events = vec![ev.clone(), ev];
        let mut source = SimulatedSource::new("dedup", events);
        let first = pipeline.next(&mut source);
        assert!(first.is_some());
        let second = pipeline.next(&mut source);
        assert!(second.is_none()); // tekrar filtrelenmeli
        assert_eq!(pipeline.stats().dedup_filtered, 1);
    }

    #[test]
    fn pipeline_run_to_completion() {
        let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
        let events = vec![
            MarketEvent::new(
                1,
                100,
                0,
                900,
                1000,
                MarketEventKind::SlotProgress { slot: 100 },
            ),
            MarketEvent::new(
                1,
                101,
                1,
                900,
                1000,
                MarketEventKind::SlotProgress { slot: 101 },
            ),
        ];
        let mut source = SimulatedSource::new("demo", events);
        let released = pipeline.run_to_completion(&mut source, |e| e.ingest_ts_ns + 50);
        assert_eq!(released.len(), 2);
    }

    #[test]
    fn pipeline_stats_dolu() {
        let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
        let stats = pipeline.stats();
        assert_eq!(stats.total_polled, 0);
        assert_eq!(stats.consumed, 0);
    }
}
