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
        let hash = tokio::task::block_in_place(|| self.rpc.get_latest_blockhash())?;
        self.cached_blockhash = Some((hash, now));
        info!("blockhash refreshed: {hash}");
        Ok(hash)
    }

    /// Force fetch a fresh blockhash, bypassing cache.
    /// Prevents replay attacks by ensuring unique blockhash per iteration.
    pub fn force_refresh(&mut self) -> Result<Hash, Box<dyn std::error::Error>> {
        let hash = tokio::task::block_in_place(|| self.rpc.get_latest_blockhash())?;
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

/// Upper bound on how long a confirmation is awaited. A blockhash stays
/// usable for ~150 slots (~60-90 s); after it expires the transaction can
/// never land, which is checked explicitly below.
const CONFIRM_DEADLINE: Duration = Duration::from_secs(90);
const CONFIRM_POLL: Duration = Duration::from_millis(500);

/// Poll `getSignatureStatuses` until the transaction reaches `confirmed`
/// commitment. Waits while `blockhash` can still land (it used to give up
/// after 3 s, so a slow-but-successful swap was reported as failed and the
/// bought tokens went unmanaged). Returns `Err` when the transaction failed
/// on-chain, when its blockhash expired without it landing, or — rarely —
/// when the deadline passed with the outcome still unknown.
pub fn confirm_transaction(
    rpc: &RpcClient,
    sig: &Signature,
    blockhash: &Hash,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut polls: u32 = 0;
    loop {
        if let Ok(statuses) = rpc.get_signature_statuses(&[*sig]) {
            if let Some(Some(status)) = statuses.value.first() {
                if let Some(err) = &status.err {
                    return Err(format!("transaction {sig} failed on-chain: {err:?}").into());
                }
                if status.confirmation_status
                    == Some(solana_transaction_status::TransactionConfirmationStatus::Confirmed)
                    || status.confirmation_status
                        == Some(solana_transaction_status::TransactionConfirmationStatus::Finalized)
                {
                    info!("transaction {sig} confirmed (slot {})", status.slot);
                    return Ok(());
                }
            }
        }
        polls += 1;
        if polls % 10 == 0 {
            let valid = rpc.is_blockhash_valid(
                blockhash,
                solana_commitment_config::CommitmentConfig::processed(),
            );
            if let Ok(false) = valid {
                // Final look: it may have landed in the last slot.
                if let Ok(st) = rpc.get_signature_statuses(&[*sig]) {
                    if let Some(Some(status)) = st.value.first() {
                        if status.err.is_none() {
                            info!("transaction {sig} landed (slot {})", status.slot);
                            return Ok(());
                        }
                    }
                }
                return Err(format!(
                    "transaction {sig} expired (blockhash no longer valid) — never landed"
                )
                .into());
            }
        }
        if started.elapsed() > CONFIRM_DEADLINE {
            return Err(format!(
                "transaction {sig} not confirmed within {}s — outcome unknown",
                CONFIRM_DEADLINE.as_secs()
            )
            .into());
        }
        std::thread::sleep(CONFIRM_POLL);
    }
}
