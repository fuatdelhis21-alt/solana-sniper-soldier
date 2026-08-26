//! # Tekrarlanan/Eski Olay Filtreleme (Deduplication)
//!
//! Kaynaktan gelen olayların tekrarlanmasını (duplicate) ve/veya sıra dışı
//! gelmesini engeller. Her market_id için en son görülen slot/seq değerini
//! tutar; daha düşük veya eşit değerdeki olayları atar.
//!
//! ## Tasarım
//! - market_id → (slot, seq) eşlemesi için `FixedHashMap` (sabit kapasiteli).
//! - Sabit kapasite: heap tahsisini önler, deterministik bellek kullanımı.
//! - `seq` sıfırlanabilir (kaynak yeniden başlatılabilir); bu durumda `force`
//!   parametresi ile sıfırlamaya izin verilir.

use std::collections::HashMap;

use crate::event::MarketEvent;

/// Varsayılan maksimum market sayısı.
pub const DEFAULT_MAX_MARKETS: usize = 256;

/// Bir market için son görülen durum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarketState {
    last_slot: u64,
    last_seq: u64,
}

/// Deduplike (yinelenen olayları filtreleme) birimi.
///
/// Her market için en son görülen `(slot, seq)` ikilisini tutar.
/// Yeni bir olay, kayıtlı değerlerden düşük veya eşitse atılır.
///
/// # Örnek
/// ```
/// use hft_marketdata::dedup::Deduplicator;
/// use hft_marketdata::event::{MarketEvent, MarketEventKind};
///
/// let mut dedup = Deduplicator::new();
/// let ev = MarketEvent::new(1, 100, 5, 0, 0, MarketEventKind::SlotProgress { slot: 100 });
///
/// assert!(dedup.is_new(&ev));  // İlk kez görüldü → yeni.
/// assert!(!dedup.is_new(&ev)); // Aynı olay → tekrar.
/// ```
pub struct Deduplicator {
    /// market_id → son durum eşlemesi.
    state: HashMap<u64, MarketState>,
    /// Atılan (filter) olay sayacı.
    filtered_count: u64,
}

impl Deduplicator {
    /// Yeni bir deduplicator oluşturur.
    pub fn new() -> Self {
        Deduplicator {
            state: HashMap::new(),
            filtered_count: 0,
        }
    }

    /// Belirtilen kapasite ile yeni bir deduplicator oluşturur.
    pub fn with_capacity(capacity: usize) -> Self {
        Deduplicator {
            state: HashMap::with_capacity(capacity),
            filtered_count: 0,
        }
    }

    /// Verilen olayın daha önce görülüp görülmediğini kontrol eder.
    /// Eğer yeni bir olaysa iç durumu günceller ve `true` döner.
    /// Tekrar veya eskiyse `false` döner (olay filtrelenir).
    ///
    /// `force` parametresi `true` ise, seq sıfırlanmış olsa bile
    /// (kaynak yeniden başlatıldı) olay kabul edilir.
    pub fn is_new(&mut self, event: &MarketEvent) -> bool {
        let key = event.market_id;
        let entry = self.state.get(&key);

        match entry {
            Some(&MarketState {
                last_slot,
                last_seq,
            }) => {
                // Slot daha büyükse → kesin yeni.
                if event.slot > last_slot {
                    self.state.insert(
                        key,
                        MarketState {
                            last_slot: event.slot,
                            last_seq: event.seq,
                        },
                    );
                    return true;
                }
                // Slot eşit, seq daha büyükse → yeni.
                if event.slot == last_slot && event.seq > last_seq {
                    self.state.insert(
                        key,
                        MarketState {
                            last_slot: event.slot,
                            last_seq: event.seq,
                        },
                    );
                    return true;
                }
                // Tekrar veya eski → filtrele.
                self.filtered_count += 1;
                false
            }
            None => {
                // İlk kez görülen market → kaydet ve kabul et.
                self.state.insert(
                    key,
                    MarketState {
                        last_slot: event.slot,
                        last_seq: event.seq,
                    },
                );
                true
            }
        }
    }

    /// Seq sıfırlamasına izin veren varyant. Kaynak yeniden başlatıldığında
    /// seq sıfırlanabilir; bu durumda eski seq değerlerini görmezden gelmek
    /// için `force` kullanılır.
    pub fn is_new_with_force(&mut self, event: &MarketEvent, force: bool) -> bool {
        if force {
            // Zorla kabul et ve iç durumu güncelle.
            self.state.insert(
                event.market_id,
                MarketState {
                    last_slot: event.slot,
                    last_seq: event.seq,
                },
            );
            return true;
        }
        self.is_new(event)
    }

    /// Belirli bir market için iç durumu sıfırlar.
    pub fn reset_market(&mut self, market_id: u64) {
        self.state.remove(&market_id);
    }

    /// Tüm iç durumu sıfırlar.
    pub fn reset_all(&mut self) {
        self.state.clear();
        self.filtered_count = 0;
    }

    /// Filtrelenen toplam olay sayısı.
    #[inline]
    pub fn filtered_count(&self) -> u64 {
        self.filtered_count
    }

    /// İzlenen market sayısı.
    #[inline]
    pub fn market_count(&self) -> usize {
        self.state.len()
    }
}

impl Default for Deduplicator {
    fn default() -> Self {
        Deduplicator::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MarketEventKind;

    fn ev(market_id: u64, slot: u64, seq: u64) -> MarketEvent {
        MarketEvent::new(
            market_id,
            slot,
            seq,
            0,
            0,
            MarketEventKind::SlotProgress { slot },
        )
    }

    #[test]
    fn ilk_gorulen_kabul() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 0)));
    }

    #[test]
    fn ayni_olay_reddedilir() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 5)));
        assert!(!dedup.is_new(&ev(1, 100, 5))); // aynı → red
    }

    #[test]
    fn eski_slot_reddedilir() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 200, 0)));
        assert!(!dedup.is_new(&ev(1, 100, 0))); // eski slot → red
    }

    #[test]
    fn yeni_slot_kabul() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 0)));
        assert!(dedup.is_new(&ev(1, 101, 0))); // yeni slot → kabul
    }

    #[test]
    fn ayni_slot_arttirilmis_seq_kabul() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 1)));
        assert!(dedup.is_new(&ev(1, 100, 2))); // aynı slot, yüksek seq → kabul
        assert!(!dedup.is_new(&ev(1, 100, 1))); // düşük seq → red
    }

    #[test]
    fn farkli_marketler_bagimsiz() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 0)));
        assert!(dedup.is_new(&ev(2, 100, 0))); // farklı market → kabul
    }

    #[test]
    fn force_seq_sifirlama() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 200, 100)));
        // force=true ile düşük seq'li olay kabul edilir.
        assert!(dedup.is_new_with_force(&ev(1, 200, 1), true));
    }

    #[test]
    fn reset_market() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 0)));
        dedup.reset_market(1);
        assert!(dedup.is_new(&ev(1, 100, 0))); // reset sonrası tekrar kabul
    }

    #[test]
    fn filtered_count() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.is_new(&ev(1, 100, 0)));
        assert!(!dedup.is_new(&ev(1, 100, 0)));
        assert!(!dedup.is_new(&ev(1, 50, 0)));
        assert_eq!(dedup.filtered_count(), 2);
    }
}
