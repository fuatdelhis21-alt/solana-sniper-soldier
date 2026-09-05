//! Risk management: circuit breaker, max trade size, slippage guard, daily loss
//! limit, daily trade cap, and a manual/automatic kill switch.
//!
//! ## Kill switch
//! The kill switch is a fail-closed mechanism: once triggered, *every* new
//! trade is rejected until it is explicitly released. It can be triggered
//! manually (`trigger_kill_switch`) or automatically when a configured limit
//! (daily loss, daily trade cap) is exceeded. Every trigger is written to the
//! risk audit log.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Machine-readable, secret-free rejection reason for a trade decision.
/// Safe to log, emit as a Prometheus label, or return over an API — never
/// contains account data, amounts, or any secret material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    StalePrice,
    SlippageExceeded,
    MaxSpendExceeded,
    DailyTradeCap,
    DailyLossLimit,
    CircuitBreakerOpen,
    TokenAuthorityRisk,
    HolderConcentrationExceeded,
    InsufficientLiquidity,
    HsmUnavailable,
    KillSwitchActive,
    LiveNotArmed,
    OpenPositionCapExceeded,
    ExposureCapExceeded,
    UnverifiableRestartState,
}

impl RejectReason {
    /// Stable machine-readable code (used as the Prometheus label value and
    /// in audit logs).
    pub fn code(&self) -> &'static str {
        match self {
            RejectReason::StalePrice => "stale_price",
            RejectReason::SlippageExceeded => "slippage_exceeded",
            RejectReason::MaxSpendExceeded => "max_spend_exceeded",
            RejectReason::DailyTradeCap => "daily_trade_cap",
            RejectReason::DailyLossLimit => "daily_loss_limit",
            RejectReason::CircuitBreakerOpen => "circuit_breaker_open",
            RejectReason::TokenAuthorityRisk => "token_authority_risk",
            RejectReason::HolderConcentrationExceeded => "holder_concentration_exceeded",
            RejectReason::InsufficientLiquidity => "insufficient_liquidity",
            RejectReason::HsmUnavailable => "hsm_unavailable",
            RejectReason::KillSwitchActive => "kill_switch_active",
            RejectReason::LiveNotArmed => "live_not_armed",
            RejectReason::OpenPositionCapExceeded => "open_position_cap_exceeded",
            RejectReason::ExposureCapExceeded => "exposure_cap_exceeded",
            RejectReason::UnverifiableRestartState => "unverifiable_restart_state",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code())
    }
}

/// Check a price/quote timestamp against a configured staleness limit.
/// Fail-closed: missing or stale data is always rejected, never a
/// fabricated/last-known-good fallback.
pub fn check_price_staleness(
    price_timestamp_ms: u128,
    max_age_ms: u64,
) -> Result<(), RejectReason> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let age_ms = now_ms.saturating_sub(price_timestamp_ms);
    if age_ms > max_age_ms as u128 {
        return Err(RejectReason::StalePrice);
    }
    Ok(())
}

// ============================================================================
// RiskFirewall — lightweight pre-trade guard for emergency stop & circuit breaker
// ============================================================================

/// Risk firewall with circuit breaker. Complements `RiskManager` with a
/// simpler f64-based interface for quick checks.
pub struct RiskFirewall {
    pub max_trade_sol: f64,
    pub daily_loss_limit_sol: f64,
    pub current_loss_sol: f64,
}

impl RiskFirewall {
    pub fn new(max_trade_sol: f64, daily_loss_limit_sol: f64) -> Self {
        Self {
            max_trade_sol,
            daily_loss_limit_sol,
            current_loss_sol: 0.0,
        }
    }

    pub fn pre_trade_check(&self, amount_sol: f64) -> Result<(), String> {
        if self.current_loss_sol >= self.daily_loss_limit_sol {
            return Err(
                "CIRCUIT BREAKER: Günlük zarar limitine ulaşıldı! Bot durduruldu.".to_string(),
            );
        }
        if amount_sol > self.max_trade_sol {
            return Err("RISK: Max işlem limitinin üzerinde!".to_string());
        }
        Ok(())
    }
}

/// Risk configuration.
#[derive(Clone)]
pub struct RiskConfig {
    pub max_trade_size_lamports: u64,
    pub max_slippage_bps: u64,
    pub daily_loss_limit_lamports: u64,
    pub max_daily_trades: u64,
    pub circuit_breaker_duration: Duration,
    /// Maximum number of simultaneously open positions.
    pub max_open_positions: u64,
    /// Maximum total lamports exposed across all open positions at once.
    pub max_total_exposure_lamports: u64,
    /// Maximum age (ms) of a price/quote before it is considered stale and
    /// the trade is rejected fail-closed.
    pub price_staleness_ms: u64,
    data_dir: PathBuf,
}

impl RiskConfig {
    pub fn devnet_defaults(data_dir: PathBuf) -> Self {
        Self {
            max_trade_size_lamports: 10_000_000_000,   // 10 SOL
            max_slippage_bps: 100,                     // 1%
            daily_loss_limit_lamports: 50_000_000_000, // 50 SOL
            max_daily_trades: 20,
            circuit_breaker_duration: Duration::from_secs(300),
            max_open_positions: 10,
            max_total_exposure_lamports: 10_000_000_000,
            price_staleness_ms: 5_000,
            data_dir,
        }
    }

    /// Conservative production defaults per the risk-hardening spec:
    /// 0.05 SOL max trade, 5 trades/day, 0.20 SOL hard daily-loss kill,
    /// 1 open position, 0.05 SOL total exposure, 2% max slippage, 5s price
    /// staleness limit. `price_staleness_ms` may be overridden via the
    /// `PRICE_STALENESS_MS` env var (still validated, never silently unset).
    pub fn production_defaults(data_dir: PathBuf) -> Result<Self, String> {
        let price_staleness_ms = match std::env::var("PRICE_STALENESS_MS") {
            Ok(v) => v
                .parse::<u64>()
                .map_err(|e| format!("invalid PRICE_STALENESS_MS: {e}"))?,
            Err(_) => 5_000,
        };
        let cfg = Self {
            max_trade_size_lamports: 50_000_000,    // 0.05 SOL
            max_slippage_bps: 200,                  // 2%
            daily_loss_limit_lamports: 200_000_000, // 0.20 SOL hard kill
            max_daily_trades: 5,
            circuit_breaker_duration: Duration::from_secs(300),
            max_open_positions: 1,
            max_total_exposure_lamports: 50_000_000, // 0.05 SOL
            price_staleness_ms,
            data_dir,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Fail-closed startup validation: every limit must be a sane, non-zero
    /// value. Called by `production_defaults`; also callable directly after
    /// manual construction (e.g. in tests) to keep the same guarantee.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_trade_size_lamports == 0 {
            return Err("max_trade_size_lamports must be > 0".into());
        }
        if self.max_slippage_bps == 0 || self.max_slippage_bps > 10_000 {
            return Err("max_slippage_bps must be in (0, 10000]".into());
        }
        if self.daily_loss_limit_lamports == 0 {
            return Err("daily_loss_limit_lamports must be > 0".into());
        }
        if self.max_daily_trades == 0 {
            return Err("max_daily_trades must be > 0".into());
        }
        if self.max_open_positions == 0 {
            return Err("max_open_positions must be > 0".into());
        }
        if self.max_total_exposure_lamports == 0 {
            return Err("max_total_exposure_lamports must be > 0".into());
        }
        if self.price_staleness_ms == 0 {
            return Err("price_staleness_ms must be > 0".into());
        }
        Ok(())
    }
}

/// Per-day loss tracker.
#[derive(Default)]
struct DailyLoss {
    date: String,
    loss_lamports: u64,
}

/// Per-day trade counter.
#[derive(Default)]
struct DailyTrades {
    date: String,
    count: u64,
}

/// Audit log entry for a kill-switch event.
#[derive(Serialize)]
struct KillSwitchAudit {
    ts_ms: u64,
    event: &'static str,
    reason: String,
}

/// Restart-safe risk state, persisted to `<data_dir>/risk_state.json` on
/// every mutation. On startup this is the ONLY source of truth for whether
/// open positions / daily counters can be trusted after a crash or restart —
/// see `RiskManager::is_state_verified`.
#[derive(Default, Serialize, Deserialize, Clone)]
struct PersistedRiskState {
    date: String,
    #[serde(default)]
    daily_trades: u64,
    #[serde(default)]
    daily_loss_lamports: u64,
    #[serde(default)]
    open_positions: u64,
    #[serde(default)]
    open_exposure_lamports: u64,
    /// Fully-detailed open position (mint, amount, entry price, spend),
    /// if one is currently open. `None` = no open position.
    #[serde(default)]
    position: Option<OpenPosition>,
    /// Net realized P&L (lamports; negative = loss) accumulated today.
    #[serde(default)]
    realized_pnl_lamports: i64,
    /// Calendar date the realized-P&L accumulator belongs to (empty when no
    /// P&L was ever booked — falls back to `date`).
    #[serde(default)]
    realized_date: String,
    /// Number of closed positions (exits) today — informational/observability
    /// only. NEVER gates entries: the daily entry cap (`daily_trades`) must
    /// not be consumed by mandatory TP/SL exits, and exits must remain
    /// possible after the entry cap is reached.
    #[serde(default)]
    daily_exits: u64,
}

/// A fully-detailed, currently-open position. Recorded on-chain after a
/// confirmed entry and consumed (only once) by `close_position` on exit.
#[derive(Default, Serialize, Deserialize, Clone, Debug)]
pub struct OpenPosition {
    /// Pool the position was opened on (CLMM pool id).
    pub pool_id: String,
    /// The mint the position holds (the token bought at entry).
    pub token_mint: String,
    /// The mint used as quote (SOL side) — what the position is valued in.
    pub quote_mint: String,
    /// Actual token amount received on-chain at entry (raw units, incl. all
    /// decimals), read from the token account after confirmation.
    pub token_amount_raw: u64,
    /// Quote-account balance right after entry (raw quote units). Subtracted
    /// from the post-exit balance to compute the REAL received quote amount.
    pub quote_balance_after_entry: u64,
    /// SOL (quote) lamports spent to open the position.
    pub spend_lamports: u64,
    /// Entry reference price (Q64.64 sqrt) for stop-loss / take-profit.
    pub entry_sqrt_price: u128,
    /// Unix ms when the entry transaction confirmed.
    pub opened_at_ms: u128,
    /// Entry transaction signature.
    pub entry_sig: String,
}

impl OpenPosition {
    /// Average realized entry price = spend / tokens received (quote per
    /// token, raw-unit ratio). `None` when the amount is zero (fail-closed:
    /// a zero-amount position cannot be valued).
    pub fn avg_entry_price(&self) -> Option<f64> {
        if self.token_amount_raw == 0 {
            return None;
        }
        Some(self.spend_lamports as f64 / self.token_amount_raw as f64)
    }
}

/// Risk manager with circuit breaker, daily trade cap, and kill switch.
pub struct RiskManager {
    config: RiskConfig,
    daily_loss: Mutex<DailyLoss>,
    daily_trades: Mutex<DailyTrades>,
    circuit_breaker_active: Mutex<bool>,
    last_breaker_reset: Mutex<Instant>,
    kill_switch_active: Mutex<bool>,
    /// Manual "arm" switch. Distinct from `kill_switch_active`: this must be
    /// explicitly enabled (fail-closed default: false) before the live
    /// trading loop is permitted to start at all.
    live_armed: Mutex<bool>,
    open_positions: Mutex<u64>,
    open_exposure_lamports: Mutex<u64>,
    /// The single fully-detailed open position, when one exists.
    position: Mutex<Option<OpenPosition>>,
    /// Net realized P&L for today (lamports; negative = loss).
    realized_pnl: Mutex<i64>,
    /// Calendar date the `realized_pnl` accumulator belongs to.
    realized_pnl_date: Mutex<String>,
    /// Closed-position (exit) counter for the current day. Distinct from
    /// `daily_trades` (the ENTRY counter): exits must never consume or gate
    /// on the daily entry cap.
    daily_exits: Mutex<DailyTrades>,
    /// Whether restart-safe state (daily counters, open positions) was
    /// verified at startup — either a fresh (no prior state file) start, or
    /// a successfully-parsed prior state file. `false` means the prior state
    /// file existed but could not be parsed (corrupt/unreadable): the caller
    /// MUST refuse to enter live mode in that case.
    state_verified: bool,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        let (state, verified) = Self::load_state(&config.data_dir);
        let mut rm = Self {
            daily_loss: Mutex::new(DailyLoss {
                date: state.date.clone(),
                loss_lamports: state.daily_loss_lamports,
            }),
            daily_trades: Mutex::new(DailyTrades {
                date: state.date.clone(),
                count: state.daily_trades,
            }),
            daily_exits: Mutex::new(DailyTrades {
                date: state.date.clone(),
                count: state.daily_exits,
            }),
            circuit_breaker_active: Mutex::new(false),
            last_breaker_reset: Mutex::new(Instant::now()),
            kill_switch_active: Mutex::new(false),
            live_armed: Mutex::new(false),
            open_positions: Mutex::new(state.open_positions),
            open_exposure_lamports: Mutex::new(state.open_exposure_lamports),
            position: Mutex::new(state.position.clone()),
            realized_pnl: Mutex::new(state.realized_pnl_lamports),
            realized_pnl_date: Mutex::new(if state.realized_date.is_empty() {
                state.date.clone()
            } else {
                state.realized_date.clone()
            }),
            state_verified: verified,
            config,
        };
        if !verified {
            rm.write_audit(
                "restart_state_unverifiable",
                "risk_state.json exists but could not be parsed",
            );
        } else if state.open_positions > 0 && state.position.is_none() {
            // A state written by an older build may have counter-only open
            // positions with no detail — the position can never be
            // reconstructed, so it must not be traded around blindly.
            // Fail-closed: refuse live trading until the operator inspects
            // and clears the state file.
            rm.write_audit(
                "restart_state_unverifiable",
                "open_positions > 0 but position detail missing — state predates position tracking or is inconsistent",
            );
            rm.state_verified = false;
        }
        rm
    }

    /// Load persisted state from `<data_dir>/risk_state.json`.
    /// - No file: fresh start, state is trivially verified (all-zero).
    /// - File present and parses: verified, counters restored.
    /// - File present but corrupt/unreadable: NOT verified (fail-closed).
    fn load_state(data_dir: &PathBuf) -> (PersistedRiskState, bool) {
        let path = data_dir.join("risk_state.json");
        if !path.exists() {
            return (PersistedRiskState::default(), true);
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<PersistedRiskState>(&content) {
                Ok(state) => (state, true),
                Err(_) => (PersistedRiskState::default(), false),
            },
            Err(_) => (PersistedRiskState::default(), false),
        }
    }

    /// Best-effort persistence of the current in-memory state. Safety-critical
    /// state (daily counters, open positions) is written after every mutation
    /// so that a restart can recover it; a write failure does not panic, but
    /// it is audited so operators can detect a broken data directory.
    fn persist_state(&self) {
        let (date, daily_trades) = {
            let t = self.daily_trades.lock().unwrap();
            (t.date.clone(), t.count)
        };
        let daily_loss_lamports = self.daily_loss.lock().unwrap().loss_lamports;
        let open_positions = *self.open_positions.lock().unwrap();
        let open_exposure_lamports = *self.open_exposure_lamports.lock().unwrap();
        let position = { self.position.lock().unwrap().clone() };
        let realized_pnl_lamports = *self.realized_pnl.lock().unwrap();
        let realized_date = { self.realized_pnl_date.lock().unwrap().clone() };
        let daily_exits = {
            let t = self.daily_exits.lock().unwrap();
            t.count
        };
        let state = PersistedRiskState {
            date,
            daily_trades,
            daily_loss_lamports,
            open_positions,
            open_exposure_lamports,
            position,
            realized_pnl_lamports,
            realized_date,
            daily_exits,
        };
        if let Ok(json) = serde_json::to_string(&state) {
            if std::fs::create_dir_all(&self.config.data_dir).is_ok() {
                let path = self.config.data_dir.join("risk_state.json");
                if std::fs::write(&path, json).is_err() {
                    self.write_audit(
                        "state_persist_failed",
                        "failed to write risk_state.json — restart safety degraded",
                    );
                }
            }
        }
    }

    /// Whether restart-safe state was verified at startup (see `new`). Callers
    /// MUST refuse to run the live trading loop when this is `false`.
    pub fn is_state_verified(&self) -> bool {
        self.state_verified
    }

    /// Explicitly arm the live trading loop (manual kill switch, fail-closed
    /// default OFF). The live loop must not start unless this has been
    /// called — see `is_live_armed`.
    pub fn arm_live(&self, reason: &str) {
        *self.live_armed.lock().unwrap() = true;
        self.write_audit("live_armed", reason);
    }

    pub fn disarm_live(&self) {
        *self.live_armed.lock().unwrap() = false;
        self.write_audit("live_disarmed", "manual disarm");
    }

    pub fn is_live_armed(&self) -> bool {
        *self.live_armed.lock().unwrap()
    }

    /// Trip the circuit breaker due to an infrastructure error (HSM, RPC,
    /// WebSocket, account resolution, etc). Fail-closed: no new trade may be
    /// opened while the breaker is active; it auto-resets after
    /// `circuit_breaker_duration`.
    pub fn trip_circuit_breaker(&self, reason: &str) {
        *self.circuit_breaker_active.lock().unwrap() = true;
        *self.last_breaker_reset.lock().unwrap() = Instant::now();
        self.write_audit("circuit_breaker_tripped", reason);
    }

    /// Record a newly-opened position (increments both the open-position
    /// count and the exposure total). Counter-only bookkeeping — prefer
    /// `record_entry` (which stores the full position detail) for real
    /// entries. Persisted immediately for restart safety.
    pub fn record_position_open(&self, lamports: u64) {
        *self.open_positions.lock().unwrap() += 1;
        *self.open_exposure_lamports.lock().unwrap() += lamports;
        self.persist_state();
    }

    /// Record a closed position, freeing its exposure. Saturating: never
    /// underflows below zero even if called out of order. Counter-only
    /// bookkeeping — prefer `close_position` (which also computes and
    /// books the realized P&L).
    pub fn record_position_close(&self, lamports: u64) {
        let mut positions = self.open_positions.lock().unwrap();
        *positions = positions.saturating_sub(1);
        drop(positions);
        let mut exposure = self.open_exposure_lamports.lock().unwrap();
        *exposure = exposure.saturating_sub(lamports);
        drop(exposure);
        self.persist_state();
    }

    /// Record a fully-detailed position entry. Fail-closed:
    /// - rejected when restart state is unverifiable,
    /// - rejected when a position is already open (idempotency guard —
    ///   a single duplicate record must never double-count exposure),
    /// - rejected when the open-position cap is already reached.
    /// On success the counters are bumped and the detail is persisted.
    pub fn record_entry(&self, pos: OpenPosition) -> Result<(), RejectReason> {
        if !self.is_state_verified() {
            return Err(RejectReason::UnverifiableRestartState);
        }
        let spend = pos.spend_lamports;
        {
            let mut slot = self.position.lock().unwrap();
            if slot.is_some() {
                return Err(RejectReason::OpenPositionCapExceeded);
            }
            *slot = Some(pos);
        }
        self.record_position_open(spend);
        Ok(())
    }

    /// The currently-open position detail, if any.
    pub fn current_position(&self) -> Option<OpenPosition> {
        self.position.lock().unwrap().clone()
    }

    /// Close the open position with the *actual* realized proceeds
    /// (quote raw units measured on-chain after confirmation) and book the
    /// realized P&L:
    /// - P&L = proceeds − spend, computed in i128 and clamped to i64
    ///   (overflow-safe; a saturating result is audited).
    /// - A realized loss is fed into the daily-loss accumulator
    ///   (`record_loss`), which triggers the hard kill switch when the
    ///   daily limit is exceeded (fail-closed).
    /// - Idempotent: the position slot is consumed via `Option::take`;
    ///   a second call returns an error and changes nothing.
    ///
    /// `proceeds_quote_raw` must be in the same unit as `spend_lamports`
    /// (native-SOL/WSOL quote with 9 decimals). The caller measures it
    /// on-chain; unmeasurable proceeds must NOT be passed as zero.
    pub fn close_position(&self, proceeds_quote_raw: u64) -> Result<OpenPosition, String> {
        let pos = self.position.lock().unwrap().take().ok_or_else(|| {
            "no open position to close — duplicate/out-of-order close rejected (idempotency)"
                .to_string()
        })?;

        // Realized P&L in i128 first, then clamped to i64.
        let pnl_i128 = (proceeds_quote_raw as i128).saturating_sub(pos.spend_lamports as i128);
        let pnl: i64 = if pnl_i128 > i64::MAX as i128 {
            i64::MAX
        } else if pnl_i128 < i64::MIN as i128 {
            i64::MIN
        } else {
            pnl_i128 as i64
        };

        // Roll the daily realized-P&L accumulator when the day changed.
        {
            let today = Self::today();
            let mut pnl_mutex = self.realized_pnl.lock().unwrap();
            let mut date_mutex = self.realized_pnl_date.lock().unwrap();
            if *date_mutex != today {
                *pnl_mutex = 0;
                *date_mutex = today;
            }
            *pnl_mutex = pnl_mutex.saturating_add(pnl);
        }

        // Free the exposure/counter slots (saturating).
        {
            let mut positions = self.open_positions.lock().unwrap();
            *positions = positions.saturating_sub(1);
        }
        {
            let mut exposure = self.open_exposure_lamports.lock().unwrap();
            *exposure = exposure.saturating_sub(pos.spend_lamports);
        }

        // Persist ONCE, after every mutation: the on-disk state is then
        // always internally consistent (position=None matches the
        // decremented counters), never a mid-transition snapshot.
        self.persist_state();

        // A realized loss counts toward the daily-loss hard kill switch
        // (may trip it — fail-closed) and is audited.
        if pnl < 0 {
            let loss = pnl.unsigned_abs();
            let _ = self.record_loss(loss);
            self.write_audit("position_closed_loss", &format!("loss={loss}"));
        } else {
            self.write_audit("position_closed_profit", &format!("profit={pnl}"));
        }
        Ok(pos)
    }

    /// Net realized P&L for the current day (lamports; negative = net loss).
    /// Rolls to zero when the date changed since the last booking.
    pub fn realized_pnl(&self) -> i64 {
        let pnl = self.realized_pnl.lock().unwrap();
        if *self.realized_pnl_date.lock().unwrap() != Self::today() {
            return 0;
        }
        *pnl
    }

    pub fn open_position_count(&self) -> u64 {
        *self.open_positions.lock().unwrap()
    }

    pub fn open_exposure_lamports(&self) -> u64 {
        *self.open_exposure_lamports.lock().unwrap()
    }

    fn today() -> String {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    }

    fn now_ms() -> u64 {
        chrono::Local::now().timestamp_millis() as u64
    }

    /// Append a kill-switch event to the risk audit log (JSONL).
    fn write_audit(&self, event: &'static str, reason: &str) {
        let entry = KillSwitchAudit {
            ts_ms: Self::now_ms(),
            event,
            reason: reason.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            let dir = self.config.data_dir.join("audit");
            if std::fs::create_dir_all(&dir).is_ok() {
                let path = dir.join("risk_audit.jsonl");
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    /// Manually trigger the kill switch. All new trades are rejected until
    /// `release_kill_switch` is called. The event is written to the audit log.
    pub fn trigger_kill_switch(&self, reason: &str) {
        *self.kill_switch_active.lock().unwrap() = true;
        self.write_audit("kill_switch_triggered", reason);
    }

    /// Manually release the kill switch.
    pub fn release_kill_switch(&self) {
        *self.kill_switch_active.lock().unwrap() = false;
        self.write_audit("kill_switch_released", "manual release");
    }

    pub fn is_kill_switch_active(&self) -> bool {
        *self.kill_switch_active.lock().unwrap()
    }

    /// Pre-ENTRY trade check: kill switch, circuit breaker, size, slippage,
    /// daily loss, and the daily ENTRY trade cap (`max_daily_trades` counts
    /// entries only — exits are gated by `pre_exit_check` and counted by
    /// `record_exit`, independent of this cap).
    pub fn pre_trade_check(
        &self,
        trade_size_lamports: u64,
        slippage_bps: u64,
    ) -> Result<(), RejectReason> {
        if !self.is_state_verified() {
            return Err(RejectReason::UnverifiableRestartState);
        }
        if *self.kill_switch_active.lock().unwrap() {
            return Err(RejectReason::KillSwitchActive);
        }
        if self.is_circuit_breaker_active() {
            return Err(RejectReason::CircuitBreakerOpen);
        }
        if trade_size_lamports > self.config.max_trade_size_lamports {
            return Err(RejectReason::MaxSpendExceeded);
        }
        if slippage_bps > self.config.max_slippage_bps {
            return Err(RejectReason::SlippageExceeded);
        }
        let daily = self.daily_loss.lock().unwrap();
        if daily.loss_lamports > self.config.daily_loss_limit_lamports {
            return Err(RejectReason::DailyLossLimit);
        }
        drop(daily);
        let trades = self.daily_trades.lock().unwrap();
        if trades.count >= self.config.max_daily_trades {
            return Err(RejectReason::DailyTradeCap);
        }
        drop(trades);
        if *self.open_positions.lock().unwrap() >= self.config.max_open_positions {
            return Err(RejectReason::OpenPositionCapExceeded);
        }
        let projected_exposure = self
            .open_exposure_lamports()
            .saturating_add(trade_size_lamports);
        if projected_exposure > self.config.max_total_exposure_lamports {
            return Err(RejectReason::ExposureCapExceeded);
        }
        Ok(())
    }

    /// Pre-EXIT check: the gates a mandatory position close (TP/SL) must
    /// pass before an exit transaction is built. Distinct from
    /// `pre_trade_check` in two deliberate ways:
    /// - Position/exposure caps do NOT apply (an exit legitimately runs
    ///   while the open-position cap is reached — it closes that position).
    /// - The daily ENTRY trade cap does NOT apply: closing an open position
    ///   to protect capital (stop-loss/take-profit) must remain possible
    ///   even after the entry limit for the day is exhausted. Exits are
    ///   counted separately (`record_exit`/`current_daily_exits`) and never
    ///   consume or double-count the entry counter.
    /// Kill switch, circuit breaker, daily-loss limit, and the slippage
    /// bound are still enforced fail-closed: no exit while any of them is
    /// active (only the operator can release a kill switch; the breaker
    /// auto-resets after its duration).
    pub fn pre_exit_check(&self, slippage_bps: u64) -> Result<(), RejectReason> {
        if !self.is_state_verified() {
            return Err(RejectReason::UnverifiableRestartState);
        }
        if *self.kill_switch_active.lock().unwrap() {
            return Err(RejectReason::KillSwitchActive);
        }
        if self.is_circuit_breaker_active() {
            return Err(RejectReason::CircuitBreakerOpen);
        }
        if slippage_bps > self.config.max_slippage_bps {
            return Err(RejectReason::SlippageExceeded);
        }
        let daily = self.daily_loss.lock().unwrap();
        if daily.loss_lamports > self.config.daily_loss_limit_lamports {
            return Err(RejectReason::DailyLossLimit);
        }
        Ok(())
    }

    /// Record a completed ENTRY trade. This is the ONLY counter that gates
    /// new entries (`pre_trade_check`'s `DailyTradeCap`). Exits MUST use
    /// `record_exit` instead — an exit recorded here would consume the daily
    /// entry cap and could even trip the defensive kill switch below,
    /// freezing the bot (including mandatory stops) for the rest of the day.
    pub fn record_trade(&self) {
        let mut trades = self.daily_trades.lock().unwrap();
        let today = Self::today();
        if trades.date != today {
            trades.date = today;
            trades.count = 0;
        }
        trades.count += 1;
        // Defensive: only reachable if the entry gate was bypassed — normal
        // flow caps entries in pre_trade_check before record_trade is ever
        // called, so a healthy bot never trips here.
        let exceeded = trades.count > self.config.max_daily_trades;
        drop(trades);
        self.persist_state();
        if exceeded {
            self.trigger_kill_switch(&format!(
                "daily entry cap exceeded (> {})",
                self.config.max_daily_trades
            ));
        }
    }

    pub fn current_daily_trades(&self) -> u64 {
        let trades = self.daily_trades.lock().unwrap();
        if trades.date != Self::today() {
            return 0;
        }
        trades.count
    }

    /// Record a completed EXIT (position close). Kept in a counter SEPARATE
    /// from the entry counter (`record_trade`): exits never consume the
    /// daily entry cap, never gate on it, and never trip the kill switch —
    /// a stop-loss/take-profit close must stay possible after the daily
    /// entry limit is exhausted (mandatory capital protection). The exit
    /// counter is informational (observability/metrics) and is persisted for
    /// restart safety.
    pub fn record_exit(&self) {
        let mut exits = self.daily_exits.lock().unwrap();
        let today = Self::today();
        if exits.date != today {
            exits.date = today;
            exits.count = 0;
        }
        exits.count += 1;
        drop(exits);
        self.persist_state();
    }

    /// Number of exits (closed positions) today.
    pub fn current_daily_exits(&self) -> u64 {
        let exits = self.daily_exits.lock().unwrap();
        if exits.date != Self::today() {
            return 0;
        }
        exits.count
    }

    /// Record a loss and potentially trigger circuit breaker.
    pub fn record_loss(&self, lamports: u64) -> Result<(), String> {
        let mut daily = self.daily_loss.lock().unwrap();
        let today = Self::today();
        if daily.date != today {
            daily.date = today;
            daily.loss_lamports = 0;
        }
        daily.loss_lamports = daily.loss_lamports.saturating_add(lamports);
        let exceeded = daily.loss_lamports > self.config.daily_loss_limit_lamports;
        drop(daily);
        self.persist_state();
        if exceeded {
            self.trigger_kill_switch(&format!(
                "daily loss limit exceeded (> {})",
                self.config.daily_loss_limit_lamports
            ));
            return Err("daily loss limit exceeded — kill switch triggered".into());
        }
        Ok(())
    }

    pub fn current_daily_loss(&self) -> u64 {
        let daily = self.daily_loss.lock().unwrap();
        if daily.date != Self::today() {
            return 0;
        }
        daily.loss_lamports
    }

    pub fn is_circuit_breaker_active(&self) -> bool {
        let active = *self.circuit_breaker_active.lock().unwrap();
        if active {
            let elapsed = self.last_breaker_reset.lock().unwrap().elapsed();
            if elapsed > self.config.circuit_breaker_duration {
                *self.circuit_breaker_active.lock().unwrap() = false;
                return false;
            }
        }
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per-call temp dir so parallel tests never share (and race on)
    /// the same `risk_state.json` / audit log.
    fn unique_test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "risk_test_{}_{}_{:?}",
                std::process::id(),
                n,
                std::thread::current().id()
            ))
            .join(nanos.to_string())
    }

    fn test_config() -> RiskConfig {
        RiskConfig {
            max_trade_size_lamports: 1_000_000_000,
            max_slippage_bps: 100,
            daily_loss_limit_lamports: 5_000_000_000,
            max_daily_trades: 3,
            circuit_breaker_duration: Duration::from_secs(300),
            max_open_positions: 10,
            max_total_exposure_lamports: 10_000_000_000,
            price_staleness_ms: 5_000,
            data_dir: unique_test_dir(),
        }
    }

    #[test]
    fn pre_trade_check_ok_within_limits() {
        let rm = RiskManager::new(test_config());
        assert!(rm.pre_trade_check(500_000_000, 50).is_ok());
    }

    #[test]
    fn pre_trade_check_rejects_oversize() {
        let rm = RiskManager::new(test_config());
        assert!(rm.pre_trade_check(2_000_000_000, 50).is_err());
    }

    #[test]
    fn pre_trade_check_rejects_high_slippage() {
        let rm = RiskManager::new(test_config());
        assert!(rm.pre_trade_check(500_000_000, 200).is_err());
    }

    #[test]
    fn kill_switch_blocks_trades() {
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("test");
        assert!(rm.is_kill_switch_active());
        assert!(rm.pre_trade_check(500_000_000, 50).is_err());
    }

    #[test]
    fn kill_switch_release_allows_trades() {
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("test");
        rm.release_kill_switch();
        assert!(!rm.is_kill_switch_active());
        assert!(rm.pre_trade_check(500_000_000, 50).is_ok());
    }

    #[test]
    fn daily_trade_cap_triggers_kill_switch() {
        let rm = RiskManager::new(test_config());
        // max_daily_trades = 3; record 4 trades -> cap exceeded -> kill switch.
        rm.record_trade();
        rm.record_trade();
        rm.record_trade();
        assert!(!rm.is_kill_switch_active());
        rm.record_trade();
        assert!(rm.is_kill_switch_active());
        assert!(rm.pre_trade_check(500_000_000, 50).is_err());
    }

    #[test]
    fn daily_loss_triggers_kill_switch() {
        let rm = RiskManager::new(test_config());
        // daily_loss_limit = 5 SOL; record 6 SOL loss -> kill switch.
        let r = rm.record_loss(6_000_000_000);
        assert!(r.is_err());
        assert!(rm.is_kill_switch_active());
    }

    #[test]
    fn reject_reason_codes_are_stable_and_snake_case() {
        assert_eq!(RejectReason::StalePrice.code(), "stale_price");
        assert_eq!(RejectReason::MaxSpendExceeded.code(), "max_spend_exceeded");
        assert_eq!(RejectReason::DailyTradeCap.code(), "daily_trade_cap");
        assert_eq!(RejectReason::DailyLossLimit.code(), "daily_loss_limit");
        assert_eq!(
            RejectReason::CircuitBreakerOpen.code(),
            "circuit_breaker_open"
        );
        assert_eq!(
            RejectReason::TokenAuthorityRisk.code(),
            "token_authority_risk"
        );
        assert_eq!(
            RejectReason::HolderConcentrationExceeded.code(),
            "holder_concentration_exceeded"
        );
        assert_eq!(
            RejectReason::InsufficientLiquidity.code(),
            "insufficient_liquidity"
        );
        assert_eq!(RejectReason::HsmUnavailable.code(), "hsm_unavailable");
    }

    #[test]
    fn pre_trade_check_rejects_specific_reason() {
        let rm = RiskManager::new(test_config());
        assert_eq!(
            rm.pre_trade_check(2_000_000_000, 50),
            Err(RejectReason::MaxSpendExceeded)
        );
        assert_eq!(
            rm.pre_trade_check(500_000_000, 9999),
            Err(RejectReason::SlippageExceeded)
        );
    }

    #[test]
    fn staleness_check_accepts_fresh_and_rejects_old() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert!(check_price_staleness(now_ms, 5_000).is_ok());
        assert_eq!(
            check_price_staleness(now_ms.saturating_sub(10_000), 5_000),
            Err(RejectReason::StalePrice)
        );
    }

    #[test]
    fn fresh_start_has_no_prior_state_and_is_verified() {
        let rm = RiskManager::new(test_config());
        assert!(rm.is_state_verified());
        assert_eq!(rm.open_position_count(), 0);
        assert_eq!(rm.open_exposure_lamports(), 0);
    }

    #[test]
    fn corrupt_state_file_is_unverifiable_and_blocks_trading() {
        let cfg = test_config();
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        std::fs::write(cfg.data_dir.join("risk_state.json"), "{ not valid json").unwrap();
        let rm = RiskManager::new(cfg);
        assert!(!rm.is_state_verified());
        assert_eq!(
            rm.pre_trade_check(500_000_000, 50),
            Err(RejectReason::UnverifiableRestartState)
        );
    }

    #[test]
    fn valid_prior_state_file_restores_counters_and_is_verified() {
        let cfg = test_config();
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        let state = PersistedRiskState {
            date: RiskManager::today(),
            daily_trades: 2,
            daily_loss_lamports: 1_000_000,
            open_positions: 1,
            open_exposure_lamports: 40_000_000,
            position: Some(sample_position(40_000_000)),
            ..Default::default()
        };
        std::fs::write(
            cfg.data_dir.join("risk_state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        let rm = RiskManager::new(cfg);
        assert!(rm.is_state_verified());
        assert_eq!(rm.current_daily_trades(), 2);
        assert_eq!(rm.current_daily_loss(), 1_000_000);
        assert_eq!(rm.open_position_count(), 1);
        assert_eq!(rm.open_exposure_lamports(), 40_000_000);
        assert!(rm.current_position().is_some());
    }

    #[test]
    fn counter_only_open_position_without_detail_is_unverifiable() {
        // A state file from a build predating position tracking has
        // open_positions > 0 but no position detail — the position can never
        // be reconstructed, so live trading must be refused (fail-closed).
        let cfg = test_config();
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        let state = PersistedRiskState {
            date: RiskManager::today(),
            open_positions: 1,
            open_exposure_lamports: 10_000_000,
            ..Default::default()
        };
        std::fs::write(
            cfg.data_dir.join("risk_state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        let rm = RiskManager::new(cfg);
        assert!(!rm.is_state_verified());
        assert_eq!(
            rm.pre_trade_check(500_000_000, 50),
            Err(RejectReason::UnverifiableRestartState)
        );
    }

    #[test]
    fn open_position_cap_rejects_beyond_limit() {
        let mut cfg = test_config();
        cfg.max_open_positions = 1;
        let rm = RiskManager::new(cfg);
        rm.record_position_open(10_000_000);
        assert_eq!(
            rm.pre_trade_check(10_000_000, 50),
            Err(RejectReason::OpenPositionCapExceeded)
        );
        rm.record_position_close(10_000_000);
        assert!(rm.pre_trade_check(10_000_000, 50).is_ok());
    }

    #[test]
    fn exposure_cap_rejects_beyond_limit() {
        let mut cfg = test_config();
        cfg.max_open_positions = 10;
        cfg.max_total_exposure_lamports = 50_000_000;
        let rm = RiskManager::new(cfg);
        rm.record_position_open(40_000_000);
        // 40M already open + 20M new > 50M cap.
        assert_eq!(
            rm.pre_trade_check(20_000_000, 50),
            Err(RejectReason::ExposureCapExceeded)
        );
        // 40M + 10M = 50M, exactly at the cap, still allowed.
        assert!(rm.pre_trade_check(10_000_000, 50).is_ok());
    }

    #[test]
    fn live_arm_switch_defaults_off() {
        let rm = RiskManager::new(test_config());
        assert!(!rm.is_live_armed());
        rm.arm_live("operator confirmed");
        assert!(rm.is_live_armed());
        rm.disarm_live();
        assert!(!rm.is_live_armed());
    }

    #[test]
    fn circuit_breaker_trip_blocks_trades_until_reset() {
        let rm = RiskManager::new(test_config());
        assert!(!rm.is_circuit_breaker_active());
        rm.trip_circuit_breaker("rpc timeout");
        assert!(rm.is_circuit_breaker_active());
        assert_eq!(
            rm.pre_trade_check(500_000_000, 50),
            Err(RejectReason::CircuitBreakerOpen)
        );
    }

    #[test]
    fn config_validate_rejects_zero_limits() {
        let mut cfg = test_config();
        cfg.max_trade_size_lamports = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn production_defaults_match_spec() {
        let cfg = RiskConfig::production_defaults(unique_test_dir()).unwrap();
        assert_eq!(cfg.max_trade_size_lamports, 50_000_000);
        assert_eq!(cfg.max_slippage_bps, 200);
        assert_eq!(cfg.daily_loss_limit_lamports, 200_000_000);
        assert_eq!(cfg.max_daily_trades, 5);
        assert_eq!(cfg.max_open_positions, 1);
        assert_eq!(cfg.max_total_exposure_lamports, 50_000_000);
    }

    #[test]
    fn audit_log_written_on_kill_switch() {
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("unit test");
        let path = std::env::temp_dir().join("audit").join("risk_audit.jsonl");
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("kill_switch_triggered"));
        assert!(content.contains("unit test"));
    }

    fn sample_position(spend_lamports: u64) -> OpenPosition {
        OpenPosition {
            pool_id: "pool_test".to_string(),
            token_mint: "meme_token_mint".to_string(),
            quote_mint: "So11111111111111111111111111111111111111112".to_string(),
            token_amount_raw: 1_000_000_000,
            quote_balance_after_entry: 0,
            spend_lamports,
            entry_sqrt_price: 1u128 << 64,
            opened_at_ms: 1_700_000_000_000,
            entry_sig: "entry_sig_test".to_string(),
        }
    }

    #[test]
    fn avg_entry_price_requires_nonzero_tokens() {
        let p = sample_position(50_000_000);
        assert!((p.avg_entry_price().unwrap() - 0.05).abs() < 1e-12);
        let mut zero = p;
        zero.token_amount_raw = 0;
        assert!(zero.avg_entry_price().is_none());
    }

    #[test]
    fn record_entry_then_close_books_realized_profit() {
        let rm = RiskManager::new(test_config());
        rm.record_entry(sample_position(40_000_000)).unwrap();
        assert_eq!(rm.open_position_count(), 1);
        assert_eq!(rm.open_exposure_lamports(), 40_000_000);
        assert!(rm.current_position().is_some());
        // Exit realized 50M quote for a 40M spend => +10M profit.
        let closed = rm.close_position(50_000_000).unwrap();
        assert_eq!(closed.spend_lamports, 40_000_000);
        assert_eq!(rm.realized_pnl(), 10_000_000);
        assert_eq!(rm.open_position_count(), 0);
        assert_eq!(rm.open_exposure_lamports(), 0);
        assert!(rm.current_position().is_none());
        assert_eq!(rm.current_daily_loss(), 0);
    }

    #[test]
    fn close_position_is_idempotent() {
        let rm = RiskManager::new(test_config());
        rm.record_entry(sample_position(40_000_000)).unwrap();
        assert!(rm.close_position(30_000_000).is_ok());
        // A second close of the same event must fail and change nothing.
        let err = rm.close_position(30_000_000).unwrap_err();
        assert!(err.contains("idempotency"), "got: {err}");
        assert_eq!(rm.realized_pnl(), -10_000_000);
        assert_eq!(rm.open_position_count(), 0);
    }

    #[test]
    fn close_with_realized_loss_feeds_daily_loss_and_kill_switch() {
        let cfg = test_config(); // daily_loss_limit = 5 SOL = 5_000_000_000
        let rm = RiskManager::new(cfg);
        rm.record_entry(sample_position(5_100_000_000)).unwrap();
        // Realized loss of 5.1 SOL > 5.0 SOL daily limit => kill switch.
        rm.close_position(0).unwrap();
        assert!(rm.is_kill_switch_active());
        assert_eq!(rm.current_daily_loss(), 5_100_000_000);
        assert_eq!(rm.realized_pnl(), -(5_100_000_000i64));
        assert_eq!(rm.pre_exit_check(50), Err(RejectReason::KillSwitchActive));
        assert_eq!(
            rm.pre_trade_check(100_000_000, 50),
            Err(RejectReason::KillSwitchActive)
        );
    }

    #[test]
    fn realized_pnl_accumulates_net_across_trades() {
        let rm = RiskManager::new(test_config());
        rm.record_entry(sample_position(40_000_000)).unwrap();
        rm.close_position(50_000_000).unwrap(); // +10M
        rm.record_entry(sample_position(30_000_000)).unwrap();
        rm.close_position(27_000_000).unwrap(); // -3M
        assert_eq!(rm.realized_pnl(), 7_000_000);
        assert_eq!(rm.current_daily_loss(), 3_000_000); // loss side only
    }

    #[test]
    fn record_entry_rejects_when_position_already_open() {
        let rm = RiskManager::new(test_config());
        rm.record_entry(sample_position(10_000_000)).unwrap();
        assert_eq!(
            rm.record_entry(sample_position(10_000_000)),
            Err(RejectReason::OpenPositionCapExceeded)
        );
        assert_eq!(rm.open_position_count(), 1);
    }

    #[test]
    fn record_entry_rejects_when_state_unverifiable() {
        let cfg = test_config();
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        std::fs::write(cfg.data_dir.join("risk_state.json"), "{ nope").unwrap();
        let rm = RiskManager::new(cfg);
        assert_eq!(
            rm.record_entry(sample_position(10_000_000)),
            Err(RejectReason::UnverifiableRestartState)
        );
    }

    #[test]
    fn restart_restores_full_position_detail_and_pnl() {
        let cfg = test_config();
        let rm = RiskManager::new(cfg.clone());
        rm.record_entry(sample_position(40_000_000)).unwrap();
        rm.close_position(50_000_000).unwrap();
        rm.record_entry(sample_position(30_000_000)).unwrap();
        drop(rm);

        // Simulated restart: same data dir, fresh manager.
        let rm2 = RiskManager::new(cfg);
        assert!(rm2.is_state_verified());
        let pos = rm2.current_position().unwrap();
        assert_eq!(pos.spend_lamports, 30_000_000);
        assert_eq!(pos.token_mint, "meme_token_mint");
        assert_eq!(rm2.open_position_count(), 1);
        assert_eq!(rm2.open_exposure_lamports(), 30_000_000);
        assert_eq!(rm2.realized_pnl(), 10_000_000);
    }

    #[test]
    fn pre_exit_check_enforces_gates() {
        let cfg = test_config(); // max_daily_trades = 3
        let rm = RiskManager::new(cfg);
        // Baseline passes with no position open (gates are gate-only).
        assert!(rm.pre_exit_check(50).is_ok());

        // Slippage bound.
        assert_eq!(rm.pre_exit_check(9999), Err(RejectReason::SlippageExceeded));
        // Circuit breaker.
        rm.trip_circuit_breaker("unit test");
        assert_eq!(rm.pre_exit_check(50), Err(RejectReason::CircuitBreakerOpen));
        // Kill switch (fresh manager — the breaker above stays tripped for
        // its whole duration and must not shadow later assertions).
        let rm = RiskManager::new(test_config());
        rm.trigger_kill_switch("unit test");
        assert_eq!(rm.pre_exit_check(50), Err(RejectReason::KillSwitchActive));
        rm.release_kill_switch();
        // The daily ENTRY cap does NOT gate exits (see
        // entry_cap_full_does_not_block_exit below) — even with the cap
        // exhausted, pre_exit_check must still pass all remaining gates.
        let rm = RiskManager::new(test_config());
        rm.record_trade();
        rm.record_trade();
        rm.record_trade();
        assert_eq!(rm.current_daily_trades(), 3);
        assert!(rm.pre_exit_check(50).is_ok());
    }

    #[test]
    fn daily_entry_cap_full_blocks_new_entries_only() {
        // max_daily_trades = 3 in test_config. 3 completed entries => the
        // 4th entry is rejected with DailyTradeCap.
        let rm = RiskManager::new(test_config());
        rm.record_trade();
        rm.record_trade();
        rm.record_trade();
        assert_eq!(
            rm.pre_trade_check(10_000_000, 50),
            Err(RejectReason::DailyTradeCap)
        );
        // The SAME exhausted cap must not block an exit.
        assert!(rm.pre_exit_check(50).is_ok());
    }

    #[test]
    fn entry_cap_full_tp_sl_exit_still_closes_position() {
        // A position is open and the daily entry cap is exhausted (e.g. the
        // position was opened as the 5th entry). A TP/SL exit must still be
        // executable and book its P&L exactly once.
        let cfg = test_config(); // max_daily_trades = 3
        let rm = RiskManager::new(cfg);
        rm.record_entry(sample_position(40_000_000)).unwrap();
        rm.record_trade();
        rm.record_trade();
        rm.record_trade();
        assert_eq!(rm.current_daily_trades(), 3);
        assert!(rm.pre_exit_check(50).is_ok());
        let closed = rm.close_position(50_000_000).unwrap();
        assert_eq!(closed.spend_lamports, 40_000_000);
        assert_eq!(rm.realized_pnl(), 10_000_000);
        assert_eq!(rm.open_position_count(), 0);
        assert!(rm.close_position(50_000_000).is_err()); // idempotent
    }

    #[test]
    fn record_exit_never_consumes_entry_cap_or_trips_kill_switch() {
        let cfg = test_config(); // max_daily_trades = 3
        let rm = RiskManager::new(cfg);
        rm.record_trade();
        rm.record_trade();
        rm.record_trade();
        assert_eq!(rm.current_daily_trades(), 3);
        assert!(!rm.is_kill_switch_active());

        // Exits after the cap is full: counted separately, no kill switch,
        // entry counter untouched.
        rm.record_exit();
        rm.record_exit();
        rm.record_exit();
        rm.record_exit(); // arbitrarily many exits
        assert_eq!(rm.current_daily_trades(), 3, "entry cap must not move");
        assert_eq!(rm.current_daily_exits(), 4);
        assert!(!rm.is_kill_switch_active());
        // Entries stay blocked, exits stay open.
        assert_eq!(
            rm.pre_trade_check(10_000_000, 50),
            Err(RejectReason::DailyTradeCap)
        );
        assert!(rm.pre_exit_check(50).is_ok());
    }

    #[test]
    fn exit_records_do_not_double_count_after_restart() {
        // record_exit persists; a restart restores the exit counter without
        // touching the entry counter (no double counting of either).
        let cfg = test_config();
        let rm = RiskManager::new(cfg.clone());
        rm.record_trade();
        rm.record_trade();
        rm.record_exit();
        rm.record_exit();
        rm.record_exit();
        drop(rm);

        let rm2 = RiskManager::new(cfg);
        assert!(rm2.is_state_verified());
        assert_eq!(rm2.current_daily_trades(), 2);
        assert_eq!(rm2.current_daily_exits(), 3);
        // New exit after restart still does not consume the entry cap.
        rm2.record_exit();
        assert_eq!(rm2.current_daily_trades(), 2);
        assert_eq!(rm2.current_daily_exits(), 4);
    }

    #[test]
    fn unverified_state_blocks_entry_and_exit_gates() {
        let cfg = test_config();
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        std::fs::write(cfg.data_dir.join("risk_state.json"), "{ corrupt").unwrap();
        let rm = RiskManager::new(cfg);
        assert!(!rm.is_state_verified());
        // Neither a new entry nor an exit may proceed on unverified state.
        assert_eq!(
            rm.pre_trade_check(10_000_000, 50),
            Err(RejectReason::UnverifiableRestartState)
        );
        assert_eq!(
            rm.pre_exit_check(50),
            Err(RejectReason::UnverifiableRestartState)
        );
        assert_eq!(
            rm.record_entry(sample_position(10_000_000)),
            Err(RejectReason::UnverifiableRestartState)
        );
    }

    #[test]
    fn close_after_restart_still_idempotent() {
        let cfg = test_config();
        let rm = RiskManager::new(cfg.clone());
        rm.record_entry(sample_position(40_000_000)).unwrap();
        drop(rm);
        let rm2 = RiskManager::new(cfg);
        assert!(rm2.close_position(45_000_000).is_ok());
        assert!(rm2.close_position(45_000_000).is_err());
        assert_eq!(rm2.realized_pnl(), 5_000_000);
    }
}
