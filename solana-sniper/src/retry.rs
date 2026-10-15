//! Retry logic for transaction submission with exponential backoff.
//!
//! Implements BlockhashManager that caches and refreshes blockhashes,
//! and send_with_retry for robust transaction delivery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use tracing::{error, info, warn};

/// Manages recent blockhash with automatic refresh.
pub struct BlockhashManager {
    rpc: Arc<RpcClient>,
    cached_blockhash: Option<(Hash, Instant)>,
    refresh_interval: Duration,
}

impl BlockhashManager {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            cached_blockhash: None,
            refresh_interval: Duration::from_secs(30),
        }
    }

    /// Returns a cached blockhash, or fetches a fresh one if stale.
    pub fn get_or_refresh(&mut self) -> Result<Hash, Box<dyn std::error::Error>> {
        let now = Instant::now();
        if let Some((hash, ts)) = &self.cached_blockhash {
            if now.duration_since(*ts) < self.refresh_interval {
                return Ok(*hash);
            }
        }
        let hash = tokio::task::block_in_place(|| {
            self.rpc.get_latest_blockhash().map_err(|e| e.to_string())
        })?;
        self.cached_blockhash = Some((hash, now));
        info!("blockhash refreshed: {hash}");
        Ok(hash)
    }

    /// Force fetch a fresh blockhash, bypassing cache.
    /// Prevents replay attacks by ensuring unique blockhash per iteration.
    pub fn force_refresh(&mut self) -> Result<Hash, Box<dyn std::error::Error>> {
        let hash = tokio::task::block_in_place(|| {
            self.rpc.get_latest_blockhash().map_err(|e| e.to_string())
        })?;
        self.cached_blockhash = Some((hash, Instant::now()));
        info!("blockhash force-refreshed: {hash}");
        Ok(hash)
    }
}

/// Send a transaction with up to `max_retries` retries and exponential backoff.
///
/// After a successful `send_transaction` (which runs preflight simulation),
/// the transaction is confirmed via `getSignatureStatuses` (confirmed
/// commitment) for as long as its blockhash can still land — see
/// `confirm_transaction`. An `Err` therefore means either "rejected/failed"
/// or "outcome unknown"; callers that move funds must reconcile from real
/// balances before concluding nothing happened.
pub fn send_with_retry(
    rpc: &RpcClient,
    tx: &Transaction,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let max_retries: u32 = 3;
    let mut attempt = 0u32;
    loop {
        match rpc.send_transaction(tx) {
            Ok(sig) => {
                info!("transaction sent: {sig}");
                match confirm_transaction(rpc, &sig, &tx.message.recent_blockhash) {
                    Ok(()) => return Ok(sig),
                    Err(e) => {
                        error!("transaction {sig} not confirmed: {e:#}");
                        return Err(e);
                    }
                }
            }
            Err(e) => {
                attempt += 1;
                if attempt >= max_retries {
                    error!("send failed after {max_retries} retries: {e:#}");
                    return Err(Box::new(e));
                }
                let delay = Duration::from_millis(200 * 2u64.pow(attempt));
                warn!("send attempt {attempt} failed: {e:#}; retrying in {delay:?}");
                std::thread::sleep(delay);
            }
        }
    }
}

/// Confirmation timing (see [`confirm_transaction`]).
#[derive(Debug, Clone, Copy)]
pub struct ConfirmTiming {
    /// After this, stop waiting — but only once the blockhash has expired.
    pub deadline: Duration,
    /// Absolute cap, used only when the blockhash validity cannot be
    /// established (e.g. the RPC keeps failing).
    pub hard_deadline: Duration,
    pub poll: Duration,
    /// Check the blockhash validity every N polls.
    pub validity_every: u32,
}

/// A blockhash stays usable for ~150 slots (~60-90 s).
pub const DEFAULT_CONFIRM_TIMING: ConfirmTiming = ConfirmTiming {
    deadline: Duration::from_secs(90),
    hard_deadline: Duration::from_secs(180),
    poll: Duration::from_millis(500),
    validity_every: 10,
};

/// Poll `getSignatureStatuses` until the transaction reaches `confirmed`
/// commitment. Returns `Err` when the transaction failed on-chain, when its
/// blockhash expired without it landing, or — rarely — when the outcome is
/// still unknown at the hard deadline.
///
/// Replay / double-entry safety: "outcome unknown" is never reported while
/// the blockhash is still valid (the transaction could still land), because
/// callers reconcile from balances and may build a NEW transaction next —
/// two live transactions would mean a double buy / sell.
pub fn confirm_transaction(
    rpc: &RpcClient,
    sig: &Signature,
    blockhash: &Hash,
) -> Result<(), Box<dyn std::error::Error>> {
    confirm_transaction_with(rpc, sig, blockhash, DEFAULT_CONFIRM_TIMING)
}

pub fn confirm_transaction_with(
    rpc: &RpcClient,
    sig: &Signature,
    blockhash: &Hash,
    timing: ConfirmTiming,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut polls: u32 = 0;
    let landed = |rpc: &RpcClient| -> Option<Result<u64, String>> {
        let statuses = rpc.get_signature_statuses(&[*sig]).ok()?;
        let status = statuses.value.first()?.as_ref()?;
        if let Some(err) = &status.err {
            return Some(Err(format!("transaction {sig} failed on-chain: {err:?}")));
        }
        matches!(
            status.confirmation_status,
            Some(solana_transaction_status::TransactionConfirmationStatus::Confirmed)
                | Some(solana_transaction_status::TransactionConfirmationStatus::Finalized)
        )
        .then_some(Ok(status.slot))
    };
    loop {
        match landed(rpc) {
            Some(Ok(slot)) => {
                info!("transaction {sig} confirmed (slot {slot})");
                return Ok(());
            }
            Some(Err(e)) => return Err(e.into()),
            None => {}
        }
        polls += 1;
        let past_deadline = started.elapsed() > timing.deadline;
        if past_deadline || polls.is_multiple_of(timing.validity_every.max(1)) {
            let valid = rpc.is_blockhash_valid(
                blockhash,
                solana_commitment_config::CommitmentConfig::processed(),
            );
            if let Ok(false) = valid {
                // Final look: it may have landed in the last slot.
                if let Some(Ok(slot)) = landed(rpc) {
                    info!("transaction {sig} landed (slot {slot})");
                    return Ok(());
                }
                return Err(format!(
                    "transaction {sig} expired (blockhash no longer valid) — never landed"
                )
                .into());
            }
            if past_deadline && started.elapsed() > timing.hard_deadline {
                return Err(format!(
                    "transaction {sig} not confirmed within {}s and its blockhash validity is unknown — outcome unknown",
                    timing.hard_deadline.as_secs()
                )
                .into());
            }
        }
        std::thread::sleep(timing.poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_rpc_client::mock_sender::MocksMap;
    use solana_rpc_client_api::request::RpcRequest;

    const FAST: ConfirmTiming = ConfirmTiming {
        deadline: Duration::from_millis(0),
        hard_deadline: Duration::from_millis(300),
        poll: Duration::from_millis(20),
        validity_every: 1,
    };

    fn validity(values: &[bool]) -> MocksMap {
        let mut m = MocksMap::default();
        for v in values {
            m.insert(
                RpcRequest::IsBlockhashValid,
                serde_json::json!({"context": {"slot": 1}, "value": v}),
            );
        }
        m
    }

    #[test]
    fn confirmed_transaction_returns_ok() {
        // The default mock reports every signature as finalized.
        let rpc = RpcClient::new_mock("succeeds".to_string());
        confirm_transaction_with(&rpc, &Signature::default(), &Hash::default(), FAST).unwrap();
    }

    #[test]
    fn expired_blockhash_is_a_definite_failure() {
        let rpc = RpcClient::new_mock_with_mocks_map("sig_not_found", validity(&[false, false]));
        let err = confirm_transaction_with(&rpc, &Signature::default(), &Hash::default(), FAST)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expired"), "{err}");
    }

    #[test]
    fn never_reports_unknown_while_the_blockhash_can_still_land() {
        // Valid for the first 5 checks, then expired: past the (zero)
        // deadline the wait continues until the blockhash is invalid, and
        // the result is the definite "expired", not "outcome unknown".
        let mut v = vec![true; 5];
        v.extend([false, false]);
        let rpc = RpcClient::new_mock_with_mocks_map("sig_not_found", validity(&v));
        let timing = ConfirmTiming {
            hard_deadline: Duration::from_secs(30),
            ..FAST
        };
        let started = Instant::now();
        let err = confirm_transaction_with(&rpc, &Signature::default(), &Hash::default(), timing)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn unknown_only_at_the_hard_deadline_when_validity_is_unreadable() {
        // No isBlockhashValid mock: every validity read fails.
        let rpc = RpcClient::new_mock_with_mocks_map("sig_not_found", MocksMap::default());
        let started = Instant::now();
        let err = confirm_transaction_with(&rpc, &Signature::default(), &Hash::default(), FAST)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outcome unknown"), "{err}");
        assert!(started.elapsed() >= FAST.hard_deadline);
    }
}
