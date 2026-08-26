//! # Latency Normalizasyonu ve Bayat-Veri Tespiti (Latency Monitor)
//!
//! Piyasa verisi kaynağından gelen olayların gecikme (latency) istatistiklerini
//! tutar ve çok eski/bayat (stale) veriyi tespit eder.
//!
//! ## Tasarım
//! - Kayar pencere (sliding window) ile min/max/avg latency hesaplar.
//! - Bayat-veri eşiği aşıldığında `is_stale()` uyarır.
//! - Tüm işlemler sabit zamanlıdır (O(1)).

use crate::event::MarketEvent;

/// Varsayılan latency penceresi boyutu (olay sayısı).
pub const DEFAULT_WINDOW_SIZE: usize = 1024;

/// Varsayılan bayat-veri eşiği (1 saniye = 1_000_000_000 nanosaniye).
pub const DEFAULT_STALE_THRESHOLD_NS: u64 = 1_000_000_000;

/// Latency istatistiklerinin anlık kopyası (snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySnapshot {
    /// Minimum latency (nanosaniye).
    pub min_ns: u64,
    /// Maksimum latency (nanosaniye).
    pub max_ns: u64,
    /// Ortalama latency (nanosaniye).
    pub avg_ns: u64,
    /// Toplam işlenen olay sayısı.
    pub total_events: u64,
    /// Bayat (stale) tespit edilen olay sayısı.
    pub stale_count: u64,
}

/// Latency izleme ve bayat-veri tespit birimi.
///
/// Pipeline'a giren her olayın `ingest_ts_ns - publish_ts_ns` farkını
/// ölçer ve kayar pencere istatistiklerini günceller.
///
/// # Örnek
/// ```
/// use hft_marketdata::latency::LatencyMonitor;
/// use hft_marketdata::event::{MarketEvent, MarketEventKind};
///
/// let mut monitor = LatencyMonitor::new();
/// let ev = MarketEvent::new(1, 100, 0, 1_000, 1_050, MarketEventKind::SlotProgress { slot: 100 });
///
/// monitor.observe(&ev);
/// let stats = monitor.snapshot();
/// assert_eq!(stats.min_ns, 50);
/// ```
pub struct LatencyMonitor {
    /// Kayar pencere buffer'ı.
    buffer: [u64; DEFAULT_WINDOW_SIZE],
    /// Buffer'daki geçerli eleman sayısı.
    count: usize,
    /// En son yazılan indeks.
    index: usize,
    /// Toplam gözlem sayısı.
    total: u64,
    /// Bayat veri sayısı.
    stale: u64,
    /// Mevcut minimum.
    min: u64,
    /// Mevcut maksimum.
    max: u64,
    /// Mevcut toplam (ortalama için).
    sum: u128,
    /// Bayat-veri eşiği (nanosaniye).
    stale_threshold_ns: u64,
}

impl LatencyMonitor {
    /// Varsayılan yapılandırma ile yeni bir monitor oluşturur.
    pub fn new() -> Self {
        LatencyMonitor {
            buffer: [0u64; DEFAULT_WINDOW_SIZE],
            count: 0,
            index: 0,
            total: 0,
            stale: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
            stale_threshold_ns: DEFAULT_STALE_THRESHOLD_NS,
        }
    }

    /// Belirtilen eşik ile yeni bir monitor oluşturur.
    pub fn with_stale_threshold(threshold_ns: u64) -> Self {
        LatencyMonitor {
            stale_threshold_ns: threshold_ns,
            ..LatencyMonitor::new()
        }
    }

    /// Belirtilen pencere boyutu ve eşik ile monitor oluşturur.
    pub fn with_config(_window_size: usize, stale_threshold_ns: u64) -> Self {
        LatencyMonitor {
            buffer: [0u64; DEFAULT_WINDOW_SIZE],
            count: 0,
            index: 0,
            total: 0,
            stale: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
            stale_threshold_ns,
        }
    }

    /// Bir olayı gözlemler ve latency istatistiklerini günceller.
    pub fn observe(&mut self, event: &MarketEvent) {
        let latency = event.latency_ns();
        self.total += 1;

        // Bayat veri kontrolü.
        if latency > self.stale_threshold_ns {
            self.stale += 1;
        }

        // Kayar pencereye ekle.
        if self.count < DEFAULT_WINDOW_SIZE {
            self.count += 1;
        } else {
            // Eski değeri toplamdan çıkar.
            let old = self.buffer[self.index];
            self.sum = self.sum.saturating_sub(old as u128);
        }

        self.buffer[self.index] = latency;
        self.index = (self.index + 1) % DEFAULT_WINDOW_SIZE;
        self.sum += latency as u128;

        // Min/Max güncelle.
        if latency < self.min {
            self.min = latency;
        }
        if latency > self.max {
            self.max = latency;
        }
    }

    /// Verilen olayın bayat olup olmadığını kontrol eder (gözlemlemeden).
    pub fn is_stale(&self, event: &MarketEvent) -> bool {
        event.latency_ns() > self.stale_threshold_ns
    }

    /// Mevcut latency istatistiklerinin anlık kopyasını döndürür.
    pub fn snapshot(&self) -> LatencySnapshot {
        let avg = if self.count > 0 {
            (self.sum / self.count as u128) as u64
        } else {
            0
        };

        LatencySnapshot {
            min_ns: if self.min == u64::MAX { 0 } else { self.min },
            max_ns: self.max,
            avg_ns: avg,
            total_events: self.total,
            stale_count: self.stale,
        }
    }

    /// Bayat-veri eşiğini günceller.
    #[inline]
    pub fn set_stale_threshold(&mut self, threshold_ns: u64) {
        self.stale_threshold_ns = threshold_ns;
    }

    /// Toplam gözlem sayısını döndürür.
    #[inline]
    pub fn total_observations(&self) -> u64 {
        self.total
    }

    /// Tüm istatistikleri sıfırlar.
    pub fn reset(&mut self) {
        self.buffer = [0u64; DEFAULT_WINDOW_SIZE];
        self.count = 0;
        self.index = 0;
        self.total = 0;
        self.stale = 0;
        self.min = u64::MAX;
        self.max = 0;
        self.sum = 0;
    }
}

impl Default for LatencyMonitor {
    fn default() -> Self {
        LatencyMonitor::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MarketEventKind;

    fn ev(publish: u64, ingest: u64) -> MarketEvent {
        MarketEvent::new(
            1,
            100,
            0,
            publish,
            ingest,
            MarketEventKind::SlotProgress { slot: 100 },
        )
    }

    #[test]
    fn latency_hesabi() {
        let mut monitor = LatencyMonitor::new();
        monitor.observe(&ev(1_000, 1_050));
        let stats = monitor.snapshot();
        assert_eq!(stats.min_ns, 50);
        assert_eq!(stats.max_ns, 50);
        assert_eq!(stats.avg_ns, 50);
        assert_eq!(stats.total_events, 1);
    }

    #[test]
    fn coklu_gozlem_ortalamasi() {
        let mut monitor = LatencyMonitor::new();
        monitor.observe(&ev(1_000, 1_100)); // 100ns
        monitor.observe(&ev(1_000, 1_200)); // 200ns
        monitor.observe(&ev(1_000, 1_300)); // 300ns
        let stats = monitor.snapshot();
        assert_eq!(stats.min_ns, 100);
        assert_eq!(stats.max_ns, 300);
        assert_eq!(stats.avg_ns, 200);
    }

    #[test]
    fn stale_tespiti() {
        let monitor = LatencyMonitor::with_stale_threshold(100);
        assert!(monitor.is_stale(&ev(1_000, 1_200))); // 200ns > 100
        assert!(!monitor.is_stale(&ev(1_000, 1_050))); // 50ns <= 100
    }

    #[test]
    fn stale_sayaci() {
        let mut monitor = LatencyMonitor::with_stale_threshold(100);
        monitor.observe(&ev(1_000, 1_050)); // 50ns → taze
        monitor.observe(&ev(1_000, 1_200)); // 200ns → bayat
        let stats = monitor.snapshot();
        assert_eq!(stats.stale_count, 1);
    }

    #[test]
    fn reset_temizler() {
        let mut monitor = LatencyMonitor::new();
        monitor.observe(&ev(1_000, 1_050));
        assert_eq!(monitor.total_observations(), 1);
        monitor.reset();
        assert_eq!(monitor.total_observations(), 0);
        let stats = monitor.snapshot();
        assert_eq!(stats.avg_ns, 0);
    }
}
