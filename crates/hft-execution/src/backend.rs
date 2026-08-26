//! # Yürütme Backend Arayüzü (Execution Backend Trait)
//!
//! Tüm yürütme backend'leri (Jito, RPC, simülasyon) tarafından
//! implemente edilen ortak arayüz.

use crate::order::{ExecutionRoute, Order};

/// `ExecutionBackend::submit()` dönüş tipleri.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitResult {
    /// Başarılı — imza (signature/bundle_id) döner.
    Ok {
        /// İşlem imzası veya bundle kimliği.
        signature: String,
    },
    /// Geçici hata — yeniden denenebilir (retry).
    Retryable {
        /// Hata detayı (loglama için).
        detail: String,
    },
    /// Kalıcı hata — yeniden denememeli.
    Permanent {
        /// Hata detayı.
        detail: String,
    },
}

/// Yürütme backend'i için soyut arayüz.
///
/// Her backend (`JitoBackend`, `RpcBackend`, simülasyon) bu trait'i
/// implemente eder. Üst katmandaki retry/fallback/circuit-breaker
/// mantığı, backend'lerin türünden bağımsız olarak çalışır.
pub trait ExecutionBackend {
    /// Bu backend'in hangi rotayı temsil ettiği.
    fn route(&self) -> ExecutionRoute;

    /// Bir emri yürütmek üzere submit eder.
    ///
    /// # Senkron Tasarım
    /// Bu fonksiyon senkron (bloklayıcı) ve saf (pure) tutulur.
    /// İmzalama ve serileştirme, submit öncesinde ayrı bir aşamada
    /// yapılır; bu fonksiyon yalnızca önceden hazırlanmış transaction
    /// byte'larını ağa iletir.
    ///
    /// # Dönüş
    /// - `SubmitResult::Ok { signature }` → başarılı.
    /// - `SubmitResult::Retryable { detail }` → geçici hata, tekrar dene.
    /// - `SubmitResult::Permanent { detail }` → kalıcı hata, tekrar deneme.
    fn submit(&mut self, order: &Order) -> SubmitResult;
}
