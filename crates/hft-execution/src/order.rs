//! # Emir (Order) Tipleri
//!
//! Yürütme (execution) katmanında kullanılan temel emir yapıları.

use serde::{Deserialize, Serialize};

/// Emir yönü — alış (Buy) veya satış (Sell).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Alış emri.
    Buy = 0,
    /// Satış emri.
    Sell = 1,
}

/// Yürütme rotası (route) — emrin hangi backend üzerinden gönderileceği.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExecutionRoute {
    /// Jito Block Engine üzerinden MEV-korumalı bundle olarak.
    JitoBundle,
    /// Standart Solana RPC üzerinden.
    Rpc,
}

/// Yürütülecek bir emri temsil eder.
///
/// `client_order_id` istemci tarafından atanır ve idempotency anahtarı
/// olarak kullanılır. Aynı `client_order_id` ile ikinci bir submit,
/// güvenli bir şekilde tekrarlanabilir (idempotent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// İstemci tarafından atanan benzersiz emir kimliği (idempotency anahtarı).
    pub client_order_id: u64,
    /// Piyasa/market tanımlayıcısı.
    pub market_id: u64,
    /// Emir yönü (alış/satış).
    pub side: Side,
    /// Emir miktarı (adet/hacim).
    pub quantity: u64,
    /// Limit fiyat (sabit noktalı, PRICE_SCALE ile ölçeklenmiş).
    pub limit_price: u64,
    /// Emrin oluşturulma zamanı (Unix nanosaniye).
    pub created_at_ns: u64,
    /// Yürütme rotası.
    pub route: ExecutionRoute,
}

impl Order {
    /// Yeni bir emir oluşturur.
    pub fn new(
        client_order_id: u64,
        market_id: u64,
        side: Side,
        quantity: u64,
        limit_price: u64,
        route: ExecutionRoute,
    ) -> Self {
        Order {
            client_order_id,
            market_id,
            side,
            quantity,
            limit_price,
            created_at_ns: 0,
            route,
        }
    }
}
