//! # Piyasa Verisi Tipleri (Market Data Types)
//!
//! Fiyat, order book ve trade gibi temel piyasa verisi yapıları.
//!
//! ## Zero-Copy Dostu Tasarım
//! - Fiyatlar `f64` yerine **sabit noktalı (fixed-point)** `u64` olarak tutulur.
//!   Bu, floating-point non-determinizmini ortadan kaldırır ve
//!   deterministik karşılaştırma sağlar.
//! - Struct'lar `#[repr(C)]` ile bellek düzeni sabittir; ileride gRPC/Geyser
//!   akışından gelen ham byte'lar üzerine güvenli `zero-copy` casting yapılabilir.
//! - Kopyalanabilir (`Copy`) küçük struct'lar sıcak yolda (hot path) heap
//!   tahsisi olmadan taşınır.

use serde::{Deserialize, Serialize};

/// Fiyatlarda kullanılan sabit noktalı ölçek. 1 birim = 1e-9 (nano ölçek),
/// Solana lamport hassasiyetiyle uyumludur. Deterministik aritmetik için
/// tüm fiyatlar bu ölçekte tam sayı (integer) olarak saklanır.
pub const PRICE_SCALE: u64 = 1_000_000_000;

/// Sabit noktalı fiyat gösterimi. Floating-point yerine `u64` kullanılarak
/// deterministik ve zero-copy dostu bir gösterim sağlanır.
///
/// # Örnek
/// ```
/// use hft_core::market::Price;
/// let p = Price::from_f64(1.5);
/// assert_eq!(p.as_f64(), 1.5);
/// assert!(Price::from_f64(2.0) > p);
/// ```
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Price(pub u64);

impl Price {
    /// Sıfır fiyat sabiti.
    pub const ZERO: Price = Price(0);

    /// Ham (raw) sabit noktalı değerden fiyat oluşturur.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Price(raw)
    }

    /// `f64` değerden fiyat oluşturur (ölçekleyerek). Sadece sınırda
    /// (giriş/çıkış) kullanılmalı; sıcak yolda `from_raw` tercih edilir.
    #[inline]
    pub fn from_f64(v: f64) -> Self {
        debug_assert!(v >= 0.0, "fiyat negatif olamaz");
        Price((v * PRICE_SCALE as f64).round() as u64)
    }

    /// Fiyatın ham sabit noktalı değerini döndürür.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// İnsan-okunur `f64` değere dönüştürür (görüntüleme/loglama için).
    #[inline]
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / PRICE_SCALE as f64
    }

    /// İki fiyat arasındaki mutlak farkı döndürür (deterministik).
    #[inline]
    pub const fn abs_diff(self, other: Price) -> u64 {
        self.0.abs_diff(other.0)
    }
}

/// İşlem miktarı (adet/hacim). Sabit noktalı, deterministik.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Quantity(pub u64);

impl Quantity {
    /// Sıfır miktar sabiti.
    pub const ZERO: Quantity = Quantity(0);

    /// Ham (raw) sabit noktalı değerden miktar oluşturur.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Quantity(raw)
    }

    /// Miktarın ham sabit noktalı değerini döndürür.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Emir yönü — alış (Bid) veya satış (Ask).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Alış tarafı.
    Bid = 0,
    /// Satış tarafı.
    Ask = 1,
}

impl Side {
    /// Karşı tarafı döndürür.
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Order book içindeki tek bir seviye (price level).
/// `#[repr(C)]` ile bellek düzeni sabit — zero-copy parse'a uygun.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLevel {
    /// Bu seviyedeki fiyat.
    pub price: Price,
    /// Bu fiyattaki toplam miktar.
    pub quantity: Quantity,
}

impl PriceLevel {
    /// Verilen fiyat ve miktardan yeni bir seviye oluşturur.
    #[inline]
    pub const fn new(price: Price, quantity: Quantity) -> Self {
        PriceLevel { price, quantity }
    }
}

/// Order book derinliği için sabit üst sınır. Sabit boyutlu diziler kullanarak
/// heap tahsisini önler ve sıcak yolda determinizm sağlar.
pub const MAX_DEPTH: usize = 16;

/// Sınırlı derinlikli (bounded) order book anlık görüntüsü (snapshot).
///
/// Sabit boyutlu dizilerle heap tahsisi olmadan çalışır. `bid_len`/`ask_len`
/// dolu seviye sayısını belirtir. Bu tasarım, sıcak yolda tahsis (allocation)
/// yapmadan order book güncellemesine olanak tanır.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    /// Piyasa/market tanımlayıcısı (pubkey/sembol hash'i).
    pub market_id: u64,
    /// Alış (bid) seviyeleri — fiyata göre azalan sırada.
    pub bids: [PriceLevel; MAX_DEPTH],
    /// Satış (ask) seviyeleri — fiyata göre artan sırada.
    pub asks: [PriceLevel; MAX_DEPTH],
    /// Dolu bid seviye sayısı.
    pub bid_len: u8,
    /// Dolu ask seviye sayısı.
    pub ask_len: u8,
    /// Kaynağın sıra numarası (sequence) — boşluk (gap) tespiti için.
    pub sequence: u64,
    /// Snapshot'ın alındığı zaman (Unix nanosaniye).
    pub timestamp_ns: u64,
}

impl OrderBook {
    /// Boş bir order book oluşturur.
    pub fn empty(market_id: u64) -> Self {
        let empty_level = PriceLevel::new(Price::ZERO, Quantity::ZERO);
        OrderBook {
            market_id,
            bids: [empty_level; MAX_DEPTH],
            asks: [empty_level; MAX_DEPTH],
            bid_len: 0,
            ask_len: 0,
            sequence: 0,
            timestamp_ns: 0,
        }
    }

    /// En iyi alış (best bid) seviyesini döndürür.
    #[inline]
    pub fn best_bid(&self) -> Option<PriceLevel> {
        if self.bid_len > 0 {
            Some(self.bids[0])
        } else {
            None
        }
    }

    /// En iyi satış (best ask) seviyesini döndürür.
    #[inline]
    pub fn best_ask(&self) -> Option<PriceLevel> {
        if self.ask_len > 0 {
            Some(self.asks[0])
        } else {
            None
        }
    }

    /// Spread'i (best_ask - best_bid) ham birimde döndürür.
    /// Her iki taraf da doluysa `Some`, aksi halde `None`.
    #[inline]
    pub fn spread(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some(a.price.raw().saturating_sub(b.price.raw())),
            _ => None,
        }
    }

    /// Orta fiyat (mid price): (best_bid + best_ask) / 2. Deterministik integer
    /// aritmetiği kullanır.
    #[inline]
    pub fn mid_price(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => {
                let mid = (b.price.raw() as u128 + a.price.raw() as u128) / 2;
                Some(Price::from_raw(mid as u64))
            }
            _ => None,
        }
    }
}

/// Gerçekleşmiş bir işlem (trade/fill) kaydı.
/// `#[repr(C)]` + `Copy` ile sıcak yolda ucuz taşınabilir.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    /// Piyasa/market tanımlayıcısı.
    pub market_id: u64,
    /// İşlemin gerçekleştiği fiyat.
    pub price: Price,
    /// İşlem miktarı.
    pub quantity: Quantity,
    /// Agresör tarafı (taker yönü).
    pub side: Side,
    /// İşlem zamanı (Unix nanosaniye).
    pub timestamp_ns: u64,
    /// Kaynağın sıra numarası.
    pub sequence: u64,
}

impl Trade {
    /// İşlemin toplam nominal değeri (price * quantity), ham ölçekte.
    /// Taşma (overflow) güvenliği için `u128` ara tip kullanılır.
    #[inline]
    pub fn notional(&self) -> u128 {
        self.price.raw() as u128 * self.quantity.raw() as u128 / PRICE_SCALE as u128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_donusum_deterministik() {
        let p = Price::from_f64(1.5);
        assert_eq!(p.raw(), 1_500_000_000);
        assert_eq!(p.as_f64(), 1.5);
    }

    #[test]
    fn side_opposite() {
        assert_eq!(Side::Bid.opposite(), Side::Ask);
        assert_eq!(Side::Ask.opposite(), Side::Bid);
    }

    #[test]
    fn orderbook_spread_ve_mid() {
        let mut ob = OrderBook::empty(1);
        ob.bids[0] = PriceLevel::new(Price::from_f64(100.0), Quantity::from_raw(10));
        ob.asks[0] = PriceLevel::new(Price::from_f64(102.0), Quantity::from_raw(10));
        ob.bid_len = 1;
        ob.ask_len = 1;

        assert_eq!(ob.spread(), Some(2 * PRICE_SCALE));
        assert_eq!(ob.mid_price(), Some(Price::from_f64(101.0)));
    }

    #[test]
    fn bos_orderbook_spread_yok() {
        let ob = OrderBook::empty(1);
        assert_eq!(ob.spread(), None);
        assert_eq!(ob.mid_price(), None);
    }

    #[test]
    fn trade_notional_hesabi() {
        let t = Trade {
            market_id: 1,
            price: Price::from_f64(2.0),
            quantity: Quantity::from_raw(5),
            side: Side::Bid,
            timestamp_ns: 0,
            sequence: 0,
        };
        // 2.0 * 5 = 10 (ham ölçekte notional / PRICE_SCALE ile normalize)
        assert_eq!(t.notional(), 10);
    }
}
