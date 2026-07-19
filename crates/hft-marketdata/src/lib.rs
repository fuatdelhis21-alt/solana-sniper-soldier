//! # hft-marketdata — Piyasa Verisi Katmanı
//!
//! Ultra-low-latency Solana trading platformunun **market data** katmanı. Ham
//! kaynak (Yellowstone/Geyser gRPC, replay veya simülasyon) olaylarını,
//! **doğrulanmış, deduplike edilmiş, sıralı** bir akışa dönüştürür ve sinyal
//! motoruna besler.
//!
//! ## Modüller
//! - [`event`]: Normalize edilmiş piyasa olayı tipleri (`MarketEvent`).
//! - [`source`]: Kaynak soyutlaması (`MarketDataSource` trait, `SimulatedSource`).
//! - [`dedup`]: Yinelenen/eski olay filtreleme (`Deduplicator`).
//! - [`latency`]: Latency normalizasyonu ve bayat-veri tespiti (`LatencyMonitor`).
//! - [`ring`]: Slot yeniden sıralama tamponu (`SlotRingBuffer`).
//! - [`pipeline`]: Uçtan uca boru hattı (`MarketDataPipeline`).
//!
//! ## Tasarım Prensipleri
//! - **Zero-trust:** Kaynak güvenilir varsayılmaz; her aşama bağımsız doğrular.
//! - **Deterministik:** Aynı girdi → aynı çıktı (replay testlerinin temeli).
//! - **Ultra-low latency:** Sabit bellek, sabit noktalı aritmetik, sıcak yolda
//!   heap tahsisi minimum.
//! - **Gözlemlenebilir:** Her aşama sayaçlarla izlenir (`PipelineStats`).
//!
//! ## Örnek
//! ```
//! use hft_marketdata::event::{MarketEvent, MarketEventKind};
//! use hft_marketdata::pipeline::{MarketDataPipeline, PipelineConfig};
//! use hft_marketdata::source::SimulatedSource;
//!
//! let mut pipeline = MarketDataPipeline::new(PipelineConfig::default());
//! let events = vec![
//!     MarketEvent::new(1, 100, 0, 900, 1000, MarketEventKind::SlotProgress { slot: 100 }),
//!     MarketEvent::new(1, 101, 0, 900, 1000, MarketEventKind::SlotProgress { slot: 101 }),
//! ];
//! let mut source = SimulatedSource::new("demo", events);
//! let released = pipeline.run_to_completion(&mut source, |e| e.ingest_ts_ns + 50);
//! assert_eq!(released.len(), 2);
//! ```

pub mod dedup;
pub mod event;
pub mod geyser;
pub mod latency;
pub mod pipeline;
pub mod ring;
pub mod source;

// Sık kullanılan tipleri kolay erişim için yeniden dışa aktar (re-export).
pub use dedup::Deduplicator;
pub use event::{MarketEvent, MarketEventKind};
pub use geyser::{GeyserConfig, GeyserError, GeyserSource, GeyserStats, GeyserStatsSnapshot};
pub use latency::LatencyMonitor;
pub use pipeline::{MarketDataPipeline, PipelineConfig, PipelineStats};
pub use ring::SlotRingBuffer;
pub use source::{MarketDataSource, SimulatedSource, SourcePoll};
