//! # Slot Yeniden Sıralama Tamponu (Slot Ring Buffer)
//!
//! Geyser gibi kaynaklardan gelen olaylar sıra dışı (out-of-order) gelebilir.
//! Bu tampon, olayları slot numarasına göre sıralayarak pipeline'ın
//! deterministik ve sıralı bir akış tüketmesini sağlar.
//!
//! ## Tasarım
//! - Sabit boyutlu dairesel tampon (ring buffer): heap tahsisi yok.
//! - Slot bazında indeksleme: `slot % capacity` pozisyonuna yazılır.
//! - `pop_ready(watermark)`: watermark'a kadar sıralı olayları döndürür.
//! - Çakışma (collision): aynı slota iki farklı olay gelirse, son gelen
//!   kazanır (diğer tipler için genişletilebilir).

use crate::event::MarketEvent;

/// Varsayılan ring buffer slot kapasitesi.
pub const DEFAULT_RING_CAPACITY: usize = 64;

/// Slot yeniden sıralama tamponu.
///
/// # Örnek
/// ```
/// use hft_marketdata::ring::SlotRingBuffer;
/// use hft_marketdata::event::{MarketEvent, MarketEventKind};
///
/// let mut ring = SlotRingBuffer::new(4);
///
/// // Sıra dışı ekleme: slot 102, 100, 101
/// ring.push(MarketEvent::new(1, 102, 0, 0, 0, MarketEventKind::SlotProgress { slot: 102 }));
/// ring.push(MarketEvent::new(1, 100, 0, 0, 0, MarketEventKind::SlotProgress { slot: 100 }));
/// ring.push(MarketEvent::new(1, 101, 0, 0, 0, MarketEventKind::SlotProgress { slot: 101 }));
///
/// // watermark=101: 100 ve 101 sıralı gelir, 102 henüz gelmez.
/// assert_eq!(ring.pop_ready(101).unwrap().slot, 100);
/// assert_eq!(ring.pop_ready(101).unwrap().slot, 101);
/// assert!(ring.pop_ready(101).is_none());
/// ```
pub struct SlotRingBuffer {
    /// Dairesel tampon: slot % capacity → olay.
    slots: Vec<Option<MarketEvent>>,
    /// Tampon kapasitesi.
    capacity: usize,
    /// Bir sonraki beklenen slot (monotonik artan watermark).
    next_expected_slot: u64,
    /// Toplam salınan (released) olay sayısı.
    released_count: u64,
}

impl SlotRingBuffer {
    /// Belirtilen kapasite ile yeni bir ring buffer oluşturur.
    ///
    /// Kapasite 2'nin katı olmalıdır (performans için). En az 2 olmalıdır.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        SlotRingBuffer {
            slots: (0..cap).map(|_| None).collect(),
            capacity: cap,
            next_expected_slot: 0,
            released_count: 0,
        }
    }

    /// Varsayılan kapasite (64) ile yeni bir ring buffer oluşturur.
    pub fn default() -> Self {
        SlotRingBuffer::new(DEFAULT_RING_CAPACITY)
    }

    /// Tampona bir olay ekler.
    ///
    /// Eğer olayın slot'u tampon kapasitesinin gerisindeyse,
    /// gecikmiş (too old) kabul edilip atılabilir.
    pub fn push(&mut self, event: MarketEvent) {
        let slot = event.slot;

        // Slot çok eskimişse atla.
        if self.next_expected_slot > 0 && slot + self.capacity as u64 <= self.next_expected_slot {
            return;
        }

        let idx = (slot as usize) % self.capacity;
        self.slots[idx] = Some(event);
    }

    /// Belirtilen watermark (dahil) değerine kadar sıralı olayları döndürür.
    ///
    /// `pop_ready`, `next_expected_slot`'tan başlayarak watermark'a kadar
    /// olan tüm slotları sırayla kontrol eder. Eğer bir slot doluysa,
    /// olayı döndürür.
    /// Boş slot varsa (atlanmış/gecikmiş), o slot atlanır.
    ///
    /// **Stale (eski) olay tespiti:** Ring buffer döngüsel olduğu için, aynı
    /// indeksteki bir önceki döngüden kalma olaylar (`event.slot < next_expected_slot`)
    /// stale kabul edilir ve sessizce atılır.
    ///
    /// Sonsuz döngüyü önlemek için en fazla `capacity` kadar adım atar.
    ///
    /// # Dönüş
    /// - `Some(event)`: Sıradaki olay (monotonik sıralı).
    /// - `None`: Watermark'a kadar işlenecek olay kalmadı.
    pub fn pop_ready(&mut self, watermark: u64) -> Option<MarketEvent> {
        let mut iterations: u64 = 0;
        let max_iterations = self.capacity as u64;

        while self.next_expected_slot <= watermark && iterations < max_iterations {
            let idx = (self.next_expected_slot as usize) % self.capacity;
            if let Some(event) = self.slots[idx].take() {
                if event.slot < self.next_expected_slot {
                    // Stale olay: bir önceki döngüden kalan. Sessizce at.
                    iterations += 1;
                    continue;
                }
                // Olayı döndür ve next_expected_slot'u güncelle.
                self.next_expected_slot = self.next_expected_slot.max(event.slot) + 1;
                self.released_count += 1;
                return Some(event);
            } else {
                // Slot boş → atla.
                self.next_expected_slot += 1;
                iterations += 1;
            }
        }
        None
    }

    /// Tampondaki tüm kalan olayları sıralı olarak boşaltır.
    pub fn drain(&mut self) -> Vec<MarketEvent> {
        let mut result = Vec::new();
        loop {
            // Tüm slotları tara.
            let mut found = false;
            for slot_opt in self.slots.iter_mut() {
                if let Some(event) = slot_opt.take() {
                    self.released_count += 1;
                    result.push(event);
                    found = true;
                }
            }
            if !found {
                break;
            }
        }
        // Slot sırasına göre sırala.
        result.sort_by_key(|e| e.slot);
        result
    }

    /// Bir sonraki beklenen slot değeri.
    #[inline]
    pub fn next_expected_slot(&self) -> u64 {
        self.next_expected_slot
    }

    /// Toplam salınan olay sayısı.
    #[inline]
    pub fn released_count(&self) -> u64 {
        self.released_count
    }

    /// Tampon kapasitesi.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Tamponu sıfırlar.
    pub fn reset(&mut self) {
        for slot in self.slots.iter_mut() {
            *slot = None;
        }
        self.next_expected_slot = 0;
        self.released_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MarketEventKind;

    fn ev(slot: u64) -> MarketEvent {
        MarketEvent::new(1, slot, 0, 0, 0, MarketEventKind::SlotProgress { slot })
    }

    #[test]
    fn sira_disi_olaylari_siralar() {
        let mut ring = SlotRingBuffer::new(4);
        ring.push(ev(102));
        ring.push(ev(100));
        ring.push(ev(101));

        assert_eq!(ring.pop_ready(101).unwrap().slot, 100);
        assert_eq!(ring.pop_ready(101).unwrap().slot, 101);
        assert!(ring.pop_ready(101).is_none());
    }

    #[test]
    fn watermark_ustu_bekler() {
        let mut ring = SlotRingBuffer::new(4);
        ring.push(ev(100));
        ring.push(ev(102));

        // watermark=100: sadece 100 gelir.
        assert_eq!(ring.pop_ready(100).unwrap().slot, 100);
        assert!(ring.pop_ready(100).is_none());

        // watermark=102: 102 gelir (101 boş → atlanır).
        assert_eq!(ring.pop_ready(102).unwrap().slot, 102);
    }

    #[test]
    fn cok_eski_slot_atilir() {
        let mut ring = SlotRingBuffer::new(4);
        ring.push(ev(100));
        ring.pop_ready(100);

        // next_expected = 101
        // Slot 100 artık geçmişte kaldı; push bu slotu kapasite kontrolünden geçirir
        // ancak pop_ready next_expected=101'den ileri taradığı için slot 100'e erişemez.
        ring.push(ev(100)); // aynı slot, farklı indeks döngüsü
                            // next_expected=101 olduğu için bu olaya erişilemez.
        assert!(ring.pop_ready(101).is_none());

        // 50 + 4 = 54 < 101 → çok eski olduğu için push tarafından atılır.
        ring.push(ev(50));
        assert!(ring.pop_ready(200).is_none());
    }

    #[test]
    fn drain_tumunu_bosaltir() {
        let mut ring = SlotRingBuffer::new(8);
        ring.push(ev(5));
        ring.push(ev(3));
        ring.push(ev(7));

        let drained = ring.drain();
        assert_eq!(drained.len(), 3);
        // Sıralı olmalı.
        assert_eq!(drained[0].slot, 3);
        assert_eq!(drained[1].slot, 5);
        assert_eq!(drained[2].slot, 7);
    }

    #[test]
    fn reset_temizler() {
        let mut ring = SlotRingBuffer::new(4);
        ring.push(ev(100));
        assert_eq!(ring.pop_ready(100).unwrap().slot, 100);
        assert_eq!(ring.released_count(), 1);

        ring.reset();
        assert_eq!(ring.released_count(), 0);
        assert_eq!(ring.next_expected_slot(), 0);
    }
}
