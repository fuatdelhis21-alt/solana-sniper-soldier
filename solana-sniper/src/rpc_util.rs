//! RPC helpers shared by the trading process and the discovery supervisor.

use std::sync::atomic::{AtomicUsize, Ordering};

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

/// The protocol limit of `getMultipleAccounts`.
pub const MAX_MULTIPLE_ACCOUNTS: usize = 100;

/// Largest `getMultipleAccounts` batch the provider accepted so far. Some
/// plans refuse larger batches with HTTP 413 (observed on a QuickNode trial
/// for 20 token accounts); the size is halved on such a refusal and kept
/// for later calls, so the refusal costs one extra request once.
static LEARNED_BATCH: AtomicUsize = AtomicUsize::new(MAX_MULTIPLE_ACCOUNTS);

/// True for the provider's "request too large" refusal (HTTP 413).
fn is_too_large(err: &str) -> bool {
    err.contains("413")
}

/// `getMultipleAccounts` in batches the provider accepts. The result has one
/// entry per key, in order. Any other error, or a 413 on a single key, is
/// returned as is: nothing is guessed.
pub fn get_multiple_accounts_adaptive(
    rpc: &RpcClient,
    keys: &[Pubkey],
) -> Result<Vec<Option<Account>>, String> {
    fetch_in_batches(keys, &LEARNED_BATCH, |chunk| {
        rpc.get_multiple_accounts(chunk).map_err(|e| e.to_string())
    })
}

/// Batching logic of [`get_multiple_accounts_adaptive`] over any fetcher
/// (unit-testable without an RPC).
pub fn fetch_in_batches<T, F>(
    keys: &[Pubkey],
    learned: &AtomicUsize,
    mut fetch: F,
) -> Result<Vec<Option<T>>, String>
where
    F: FnMut(&[Pubkey]) -> Result<Vec<Option<T>>, String>,
{
    let mut out: Vec<Option<T>> = Vec::with_capacity(keys.len());
    let mut i = 0;
    while i < keys.len() {
        let size = learned
            .load(Ordering::Relaxed)
            .clamp(1, MAX_MULTIPLE_ACCOUNTS)
            .min(keys.len() - i);
        let chunk = &keys[i..i + size];
        match fetch(chunk) {
            Ok(v) if v.len() == chunk.len() => {
                out.extend(v);
                i += size;
            }
            Ok(v) => {
                return Err(format!(
                    "getMultipleAccounts returned {} entries for {} keys",
                    v.len(),
                    chunk.len()
                ))
            }
            Err(e) if is_too_large(&e) && size > 1 => {
                learned.store(size / 2, Ordering::Relaxed);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: usize) -> Vec<Pubkey> {
        (0..n).map(|_| Pubkey::new_unique()).collect()
    }

    #[test]
    fn halves_on_413_and_remembers_the_size() {
        let learned = AtomicUsize::new(MAX_MULTIPLE_ACCOUNTS);
        let ks = keys(20);
        let mut calls = Vec::new();
        // The provider accepts at most 5 keys per request.
        let fetch = |c: &[Pubkey]| {
            calls.push(c.len());
            if c.len() > 5 {
                Err("HTTP status client error (413 Request Entity Too Large)".to_string())
            } else {
                Ok(c.iter().map(|k| Some(*k)).collect())
            }
        };
        let out = fetch_in_batches(&ks, &learned, fetch).unwrap();
        assert_eq!(
            out,
            ks.iter().map(|k| Some(*k)).collect::<Vec<_>>(),
            "order kept"
        );
        assert_eq!(learned.load(Ordering::Relaxed), 5);
        assert_eq!(calls, vec![20, 10, 5, 5, 5, 5]);
        // Next call starts at the learned size: no refused request.
        let mut calls2 = Vec::new();
        fetch_in_batches(&ks, &learned, |c: &[Pubkey]| {
            calls2.push(c.len());
            Ok(c.iter().map(|_| Some(0u8)).collect())
        })
        .unwrap();
        assert_eq!(calls2, vec![5, 5, 5, 5]);
    }

    #[test]
    fn other_errors_and_bad_responses_fail_closed() {
        let learned = AtomicUsize::new(MAX_MULTIPLE_ACCOUNTS);
        let ks = keys(4);
        let e = fetch_in_batches::<u8, _>(&ks, &learned, |_| Err("HTTP 429".into())).unwrap_err();
        assert!(e.contains("429"));
        assert_eq!(
            learned.load(Ordering::Relaxed),
            MAX_MULTIPLE_ACCOUNTS,
            "429 never shrinks"
        );
        // 413 even for one key: give up, do not loop.
        let e = fetch_in_batches::<u8, _>(&ks, &learned, |_| Err("413".into())).unwrap_err();
        assert!(e.contains("413"));
        assert_eq!(learned.load(Ordering::Relaxed), 1);
        // A response with the wrong number of entries is an error.
        let learned = AtomicUsize::new(MAX_MULTIPLE_ACCOUNTS);
        let e = fetch_in_batches(&ks, &learned, |_| Ok(vec![Some(1u8)])).unwrap_err();
        assert!(e.contains("returned 1 entries for 4 keys"), "{e}");
        // Empty input: no request at all.
        let out: Vec<Option<u8>> =
            fetch_in_batches(&[], &learned, |_| panic!("no call expected")).unwrap();
        assert!(out.is_empty());
    }
}
