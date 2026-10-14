//! New-pool discovery for the `pool-sniper` supervisor (pure logic, no I/O).
//!
//! The supervisor subscribes to the Raydium CLMM program's logs, detects
//! `CreatePool`, resolves the new pool's accounts from the transaction and
//! keeps it on a watch list: a pool has no liquidity at creation, so it is
//! re-checked until it qualifies (wSOL pair, classic SPL mint with revoked
//! authorities, enough SOL-side liquidity) or the watch time runs out. A
//! qualifying pool is handed to one `solana-sniper --single-trade` process,
//! which applies every regular entry/exit gate on its own.

use std::collections::{HashMap, HashSet, VecDeque};

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;

/// Log line the CLMM program emits for its `create_pool` instruction.
pub const CREATE_POOL_LOG: &str = "Program log: Instruction: CreatePool";

/// Account positions in the CLMM `create_pool` instruction:
/// 0 pool_creator, 1 amm_config, 2 pool_state, 3 token_mint_0,
/// 4 token_mint_1, 5 token_vault_0, 6 token_vault_1, ...
const IDX_POOL_STATE: usize = 2;
const IDX_MINT_0: usize = 3;
const IDX_MINT_1: usize = 4;

/// Anchor discriminator of `create_pool`: sha256("global:create_pool")[..8].
pub fn create_pool_discriminator() -> [u8; 8] {
    let h = Sha256::digest(b"global:create_pool");
    let mut d = [0u8; 8];
    d.copy_from_slice(&h[..8]);
    d
}

/// True when a (successful) transaction's logs contain a CLMM CreatePool.
pub fn logs_contain_create_pool(logs: &[String]) -> bool {
    logs.iter().any(|l| l == CREATE_POOL_LOG)
}

/// A pool announced by a CreatePool instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewPool {
    pub pool_id: Pubkey,
    pub mint_0: Pubkey,
    pub mint_1: Pubkey,
}

/// One compiled instruction of a transaction (top-level or inner), with its
/// program and accounts already resolved to pubkeys.
pub struct RawInstruction<'a> {
    pub program_id: Pubkey,
    pub accounts: Vec<Pubkey>,
    pub data: &'a [u8],
}

/// Extract the CreatePool instructions addressed to `program_id`.
pub fn find_create_pools(program_id: &Pubkey, instructions: &[RawInstruction]) -> Vec<NewPool> {
    let disc = create_pool_discriminator();
    instructions
        .iter()
        .filter(|ix| ix.program_id == *program_id && ix.data.len() >= 8 && ix.data[..8] == disc)
        .filter_map(|ix| {
            Some(NewPool {
                pool_id: *ix.accounts.get(IDX_POOL_STATE)?,
                mint_0: *ix.accounts.get(IDX_MINT_0)?,
                mint_1: *ix.accounts.get(IDX_MINT_1)?,
            })
        })
        .collect()
}

/// Resolve a compiled instruction's account indexes against the full key
/// list (static keys followed by loaded writable + readonly ALT addresses).
pub fn resolve_indexes(keys: &[Pubkey], indexes: &[u8]) -> Option<Vec<Pubkey>> {
    indexes
        .iter()
        .map(|&i| keys.get(i as usize).copied())
        .collect()
}

/// The token to buy when the pool pairs it with `base` (wSOL); `None` for
/// pools without the base mint (cannot be entered with SOL).
pub fn token_side(pool: &NewPool, base: &Pubkey) -> Option<Pubkey> {
    match (pool.mint_0 == *base, pool.mint_1 == *base) {
        (true, false) => Some(pool.mint_1),
        (false, true) => Some(pool.mint_0),
        _ => None,
    }
}

/// Result of one candidate check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// All pre-filters pass: start a trading process for it.
    Ready,
    /// Not yet (e.g. liquidity still below the floor): check again later.
    Retry(String),
    /// Never tradable (authority, Token-2022, wrong pair): drop it.
    Reject(String),
}

/// A pool on the watch list.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub pool: NewPool,
    /// The non-wSOL mint (what gets bought).
    pub token_mint: Pubkey,
    pub discovered_at_ms: u64,
    pub next_check_ms: u64,
    pub checks: u32,
    /// Authority / program checks passed once (they never change back).
    pub mint_ok: bool,
    pub last_reason: String,
}

/// Watch list of discovered pools, oldest first. Pure: every method takes
/// the current time from the caller.
pub struct WatchList {
    items: VecDeque<Candidate>,
    seen: HashSet<Pubkey>,
    max_len: usize,
    watch_ms: u64,
    recheck_ms: u64,
}

impl WatchList {
    pub fn new(max_len: usize, watch_ms: u64, recheck_ms: u64) -> Self {
        Self {
            items: VecDeque::new(),
            seen: HashSet::new(),
            max_len: max_len.max(1),
            watch_ms,
            recheck_ms: recheck_ms.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Add a pool once (a pool id is never re-added, even after it left the
    /// list). The oldest entry is dropped when the list is full. Returns
    /// whether it was added.
    pub fn add(&mut self, pool: NewPool, token_mint: Pubkey, now_ms: u64) -> bool {
        if !self.seen.insert(pool.pool_id) {
            return false;
        }
        if self.items.len() >= self.max_len {
            self.items.pop_front();
        }
        self.items.push_back(Candidate {
            pool,
            token_mint,
            discovered_at_ms: now_ms,
            next_check_ms: now_ms,
            checks: 0,
            mint_ok: false,
            last_reason: String::new(),
        });
        true
    }

    /// Mark a pool as already handled (e.g. a resumed live position) so a
    /// late CreatePool notification never re-adds it.
    pub fn mark_seen(&mut self, pool_id: Pubkey) {
        self.seen.insert(pool_id);
        self.items.retain(|c| c.pool.pool_id != pool_id);
    }

    /// Drop candidates whose watch time is over; returns them.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Candidate> {
        let watch_ms = self.watch_ms;
        let (gone, keep): (Vec<_>, Vec<_>) = self
            .items
            .drain(..)
            .partition(|c| now_ms.saturating_sub(c.discovered_at_ms) >= watch_ms);
        self.items = keep.into();
        gone
    }

    /// Oldest candidate due for a check, removed from the list. Return it
    /// with [`WatchList::reschedule`] when the check says `Retry`.
    pub fn take_due(&mut self, now_ms: u64) -> Option<Candidate> {
        let pos = self.items.iter().position(|c| c.next_check_ms <= now_ms)?;
        self.items.remove(pos)
    }

    pub fn reschedule(&mut self, mut c: Candidate, now_ms: u64, reason: String) {
        c.checks += 1;
        c.next_check_ms = now_ms + self.recheck_ms;
        c.last_reason = reason;
        self.items.push_back(c);
    }
}

/// Per-UTC-day counter of started trading processes.
#[derive(Debug, Default)]
pub struct DailyCap {
    day: i64,
    count: u32,
}

impl DailyCap {
    const DAY_MS: u64 = 86_400_000;

    pub fn count(&mut self, now_ms: u64) -> u32 {
        self.roll(now_ms);
        self.count
    }

    pub fn reached(&mut self, now_ms: u64, max: u32) -> bool {
        self.count(now_ms) >= max
    }

    pub fn record(&mut self, now_ms: u64) {
        self.roll(now_ms);
        self.count += 1;
    }

    fn roll(&mut self, now_ms: u64) {
        let day = (now_ms / Self::DAY_MS) as i64;
        if day != self.day {
            self.day = day;
            self.count = 0;
        }
    }
}

/// Signatures waiting for `getTransaction` (a just-confirmed transaction is
/// often not served yet): retried a few times, then dropped.
#[derive(Default)]
pub struct PendingSignatures {
    attempts: HashMap<String, u32>,
    order: VecDeque<String>,
}

impl PendingSignatures {
    pub const MAX_ATTEMPTS: u32 = 5;
    const MAX_LEN: usize = 500;

    /// Queue a signature (duplicates ignored). Returns `true` when the queue
    /// was full and the oldest pending signature was evicted (a lost event:
    /// counted by the caller).
    pub fn push(&mut self, sig: String) -> bool {
        if self.attempts.contains_key(&sig) {
            return false;
        }
        let mut evicted = false;
        if self.order.len() >= Self::MAX_LEN {
            if let Some(old) = self.order.pop_front() {
                self.attempts.remove(&old);
                evicted = true;
            }
        }
        self.attempts.insert(sig.clone(), 0);
        self.order.push_back(sig);
        evicted
    }

    pub fn pop(&mut self) -> Option<String> {
        let sig = self.order.pop_front()?;
        Some(sig)
    }

    /// Put a signature back after a failed fetch; false once it ran out of
    /// attempts (dropped).
    pub fn retry(&mut self, sig: String) -> bool {
        let n = self.attempts.entry(sig.clone()).or_insert(0);
        *n += 1;
        if *n >= Self::MAX_ATTEMPTS {
            self.attempts.remove(&sig);
            return false;
        }
        self.order.push_back(sig);
        true
    }

    pub fn done(&mut self, sig: &str) {
        self.attempts.remove(sig);
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn pool(n: u8, m0: Pubkey, m1: Pubkey) -> NewPool {
        NewPool {
            pool_id: pk(n),
            mint_0: m0,
            mint_1: m1,
        }
    }

    #[test]
    fn discriminator_is_anchor_create_pool() {
        // sha256("global:create_pool")[..8], as in the Raydium CLMM IDL.
        assert_eq!(
            create_pool_discriminator(),
            [233, 146, 209, 142, 207, 104, 64, 188]
        );
    }

    #[test]
    fn detects_create_pool_log_only_exactly() {
        let logs = vec![
            "Program CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK invoke [1]".to_string(),
            CREATE_POOL_LOG.to_string(),
        ];
        assert!(logs_contain_create_pool(&logs));
        assert!(!logs_contain_create_pool(&[
            "Program log: Instruction: SwapV2".to_string()
        ]));
        assert!(!logs_contain_create_pool(&[
            "Program log: Instruction: CreatePoolX".to_string()
        ]));
    }

    #[test]
    fn finds_create_pool_accounts_and_ignores_other_instructions() {
        let program = pk(200);
        let mut data = create_pool_discriminator().to_vec();
        data.extend_from_slice(&[1, 2, 3]);
        let accounts: Vec<Pubkey> = (0..13).map(pk).collect();
        let swap_data = [9u8; 16];
        let ixs = vec![
            RawInstruction {
                program_id: program,
                accounts: accounts.clone(),
                data: &swap_data,
            },
            RawInstruction {
                program_id: pk(201),
                accounts: accounts.clone(),
                data: &data,
            },
            RawInstruction {
                program_id: program,
                accounts: accounts.clone(),
                data: &data,
            },
            RawInstruction {
                program_id: program,
                accounts: accounts[..3].to_vec(),
                data: &data,
            },
        ];
        let found = find_create_pools(&program, &ixs);
        assert_eq!(found, vec![pool(2, pk(3), pk(4))]);
    }

    #[test]
    fn resolves_indexes_including_lookup_table_keys() {
        let keys = vec![pk(0), pk(1), pk(2)];
        assert_eq!(resolve_indexes(&keys, &[2, 0]), Some(vec![pk(2), pk(0)]));
        assert_eq!(resolve_indexes(&keys, &[3]), None);
    }

    #[test]
    fn token_side_requires_exactly_one_base_mint() {
        let base = pk(100);
        assert_eq!(token_side(&pool(1, base, pk(7)), &base), Some(pk(7)));
        assert_eq!(token_side(&pool(1, pk(7), base), &base), Some(pk(7)));
        assert_eq!(token_side(&pool(1, pk(7), pk(8)), &base), None);
        assert_eq!(token_side(&pool(1, base, base), &base), None);
    }

    #[test]
    fn watch_list_dedups_rechecks_and_expires() {
        let mut w = WatchList::new(2, 1_000, 100);
        assert!(w.add(pool(1, pk(9), pk(10)), pk(10), 0));
        assert!(!w.add(pool(1, pk(9), pk(10)), pk(10), 5), "duplicate pool");
        let c = w.take_due(0).unwrap();
        assert!(w.take_due(0).is_none());
        w.reschedule(c, 0, "liquidity".into());
        assert!(w.take_due(50).is_none(), "not due before recheck interval");
        let c = w.take_due(100).unwrap();
        assert_eq!(c.checks, 1);
        assert_eq!(c.last_reason, "liquidity");
        w.reschedule(c, 100, "liquidity".into());
        assert_eq!(w.expire(999).len(), 0);
        assert_eq!(w.expire(1_000).len(), 1);
        assert!(w.is_empty());
        assert!(
            !w.add(pool(1, pk(9), pk(10)), pk(10), 2_000),
            "never re-added"
        );
    }

    #[test]
    fn watch_list_drops_oldest_when_full_and_mark_seen_blocks() {
        let mut w = WatchList::new(2, 10_000, 100);
        w.add(pool(1, pk(9), pk(10)), pk(10), 0);
        w.add(pool(2, pk(9), pk(11)), pk(11), 1);
        w.add(pool(3, pk(9), pk(12)), pk(12), 2);
        assert_eq!(w.len(), 2);
        assert_eq!(w.take_due(10).unwrap().pool.pool_id, pk(2));
        w.mark_seen(pk(3));
        assert!(w.is_empty());
        w.mark_seen(pk(4));
        assert!(!w.add(pool(4, pk(9), pk(13)), pk(13), 20));
    }

    #[test]
    fn daily_cap_resets_each_utc_day() {
        let mut cap = DailyCap::default();
        let day = 86_400_000;
        cap.record(day * 10 + 5);
        cap.record(day * 10 + 6);
        assert!(cap.reached(day * 10 + 7, 2));
        assert!(!cap.reached(day * 11, 2));
        assert_eq!(cap.count(day * 11), 0);
    }

    #[test]
    fn pending_signatures_report_evictions() {
        let mut p = PendingSignatures::default();
        for i in 0..PendingSignatures::MAX_LEN {
            assert!(!p.push(format!("s{i}")));
        }
        assert!(!p.push("s0".into()), "duplicate is not an eviction");
        assert!(p.push("new".into()), "full queue evicts the oldest");
        assert_eq!(p.len(), PendingSignatures::MAX_LEN);
    }

    #[test]
    fn pending_signatures_retry_then_drop() {
        let mut p = PendingSignatures::default();
        p.push("a".into());
        p.push("a".into());
        assert_eq!(p.len(), 1);
        let s = p.pop().unwrap();
        for _ in 0..PendingSignatures::MAX_ATTEMPTS - 1 {
            assert!(p.retry(s.clone()));
            assert_eq!(p.pop().as_deref(), Some("a"));
        }
        assert!(!p.retry(s));
        assert!(p.is_empty());
    }
}
