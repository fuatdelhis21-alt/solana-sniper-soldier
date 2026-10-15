//! Authoritative swap quotes from an on-chain simulation.
//!
//! The adapter's `quote()` uses the pool's spot price: it ignores price
//! impact and tick crossings, which on a thin, new concentrated-liquidity
//! pool can overstate the executable output by a wide margin. Rather than
//! re-implementing Raydium's swap math (tick traversal, fee growth, rounding)
//! off-chain, the exact output is taken from the deployed program itself:
//! the real swap transaction (with `minimum_amount_out = 1`, no unwrap / tip)
//! is run through `simulateTransaction`, and the post-simulation balance of
//! the output token account is read from the simulated state.
//!
//! Everything money-related here is integer arithmetic.

use base64::Engine;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;

/// Exact output of a simulated swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimQuote {
    /// Raw units of the output mint the swap delivers.
    pub amount_out: u64,
    pub compute_units: Option<u64>,
}

/// SPL token account amount (bytes 64..72) from raw account data.
pub fn token_amount(data: &[u8]) -> Option<u64> {
    data.get(64..72)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
}

/// `minimum_amount_out` for an exact quote and a slippage tolerance, in
/// integer math (rounded down). Never below 1: a zero minimum would accept
/// any execution price.
pub fn min_out(exact_out: u64, slippage_bps: u64) -> u64 {
    let keep = 10_000u128.saturating_sub(slippage_bps.min(10_000) as u128);
    ((exact_out as u128 * keep / 10_000) as u64).max(1)
}

/// Price impact (bps) of the executable output against the spot-price
/// output (fee already applied in both): 0 when execution is at or above
/// spot. `spot_out == 0` (no spot price) is treated as maximal impact.
pub fn price_impact_bps(spot_out: u64, exact_out: u64) -> u64 {
    if spot_out == 0 {
        return 10_000;
    }
    if exact_out >= spot_out {
        return 0;
    }
    ((spot_out - exact_out) as u128 * 10_000 / spot_out as u128) as u64
}

/// Inputs of the round-trip edge estimate. All amounts are integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeInputs {
    /// Gain targeted by the take-profit (bps of the position).
    pub take_profit_bps: u64,
    /// Pool trade fee (ppm), paid on BOTH legs.
    pub fee_rate_ppm: u64,
    /// Measured price impact of the entry (exact simulation vs spot).
    pub entry_impact_bps: u64,
    /// Exit impact estimate as a percentage of the entry impact (>= 100).
    /// The exit cannot be simulated before the tokens are held, and a thin
    /// launch pool usually loses liquidity after the entry, so the exit is
    /// assumed to cost MORE than the entry, never less.
    pub exit_impact_pct: u64,
    /// Base + priority fee (+ tip) of ONE swap transaction, lamports.
    pub network_cost_per_tx_lamports: u64,
    /// Extra failed transactions budgeted for (each costs one network fee):
    /// a failed exit still pays its fees.
    pub failed_tx_reserve: u64,
    /// Exit slippage tolerance (bps). Kept apart from impact: it is the
    /// worst accepted execution, not the expected one.
    pub exit_slippage_bps: u64,
    /// Position size, lamports.
    pub position_lamports: u64,
}

/// Round-trip edge breakdown (bps of the position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct EdgeBreakdown {
    pub expected_return_bps: i64,
    pub fee_cost_bps: i64,
    pub entry_impact_bps: i64,
    /// Estimated (not simulated) exit impact.
    pub exit_impact_bps: i64,
    pub network_cost_bps: i64,
    pub failed_tx_reserve_bps: i64,
    /// Everything the exit leg is expected to cost: its fee, impact and
    /// network fee (+ the failed-tx reserve).
    pub exit_cost_bps: i64,
    /// Expected return minus all expected costs.
    pub net_edge_bps: i64,
    /// Net edge if the exit also fills at the slippage limit.
    pub worst_case_edge_bps: i64,
}

fn bps_of(lamports: u128, position_lamports: u128) -> i128 {
    (lamports.saturating_mul(10_000) / position_lamports).min(i64::MAX as u128) as i128
}

/// Expected net edge of a round trip that reaches the take-profit, with the
/// exit costed separately and conservatively. `None` (= never trade) for a
/// zero-size position. Integer math in i128, clamped into i64: no input can
/// overflow it.
pub fn edge_breakdown(i: &EdgeInputs) -> Option<EdgeBreakdown> {
    if i.position_lamports == 0 {
        return None;
    }
    let pos = i.position_lamports as u128;
    let fee_leg = (i.fee_rate_ppm / 100) as i128; // ppm -> bps
    let entry_impact = i.entry_impact_bps as i128;
    let exit_impact = entry_impact.saturating_mul(i.exit_impact_pct.max(100) as i128) / 100;
    let net_leg = bps_of(i.network_cost_per_tx_lamports as u128, pos);
    let reserve = bps_of(
        (i.network_cost_per_tx_lamports as u128).saturating_mul(i.failed_tx_reserve as u128),
        pos,
    );
    let expected = i.take_profit_bps as i128;
    let exit_cost = fee_leg + exit_impact + net_leg + reserve;
    let net = expected - (fee_leg + entry_impact + net_leg) - exit_cost;
    let worst = net - i.exit_slippage_bps as i128;
    let c = |v: i128| v.clamp(i64::MIN as i128 / 2, i64::MAX as i128 / 2) as i64;
    Some(EdgeBreakdown {
        expected_return_bps: c(expected),
        fee_cost_bps: c(2 * fee_leg),
        entry_impact_bps: c(entry_impact),
        exit_impact_bps: c(exit_impact),
        network_cost_bps: c(2 * net_leg),
        failed_tx_reserve_bps: c(reserve),
        exit_cost_bps: c(exit_cost),
        net_edge_bps: c(net),
        worst_case_edge_bps: c(worst),
    })
}

/// Position size relative to the pool's SOL-side liquidity (bps). `None`
/// when the liquidity is unknown or zero: the caller must not trade.
pub fn position_liquidity_bps(position_lamports: u64, liquidity_lamports: u64) -> Option<u64> {
    if liquidity_lamports == 0 {
        return None;
    }
    let r = position_lamports as u128 * 10_000 / liquidity_lamports as u128;
    Some(r.min(u64::MAX as u128) as u64)
}

/// Estimated costs of a paper round trip (lamports). Paper mode never builds
/// a transaction, so nothing here is measured: it is a deliberately
/// pessimistic estimate so paper P&L is not the raw spot-price move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PaperCost {
    pub fee_lamports: u64,
    pub entry_impact_lamports: u64,
    pub exit_impact_lamports: u64,
    /// Adverse fill from the decision-to-landing delay (both legs).
    pub latency_lamports: u64,
    pub network_lamports: u64,
    pub total_lamports: u64,
}

/// Paper cost model inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaperCostModel {
    pub fee_rate_ppm: u64,
    /// Exit impact as a % of the constant-product estimate (min 100).
    pub exit_impact_pct: u64,
    /// Adverse price move per leg between decision and fill (bps).
    pub latency_bps_per_leg: u64,
    /// Base + priority (+ tip) of one transaction.
    pub network_cost_per_tx_lamports: u64,
}

/// Constant-product impact estimate (lamports) of trading `amount` against
/// `liquidity` on the SOL side: `amount^2 / (liquidity + amount)`. Unknown
/// (zero) liquidity costs the whole amount.
pub fn cp_impact_lamports(amount: u64, liquidity_lamports: u64) -> u64 {
    if liquidity_lamports == 0 {
        return amount;
    }
    let a = amount as u128;
    (a * a / (liquidity_lamports as u128 + a)) as u64
}

/// Round-trip cost estimate of a paper position of `position_lamports`
/// entered at `entry_liquidity` and exited at `exit_liquidity`.
pub fn paper_round_trip_cost(
    m: &PaperCostModel,
    position_lamports: u64,
    entry_liquidity_lamports: u64,
    exit_liquidity_lamports: u64,
) -> PaperCost {
    let pos = position_lamports as u128;
    let fee = 2 * pos * m.fee_rate_ppm.min(1_000_000) as u128 / 1_000_000;
    let entry_impact = cp_impact_lamports(position_lamports, entry_liquidity_lamports) as u128;
    let exit_impact = cp_impact_lamports(position_lamports, exit_liquidity_lamports) as u128
        * m.exit_impact_pct.clamp(100, 10_000) as u128
        / 100;
    let latency = 2 * pos * m.latency_bps_per_leg.min(10_000) as u128 / 10_000;
    let network = 2 * m.network_cost_per_tx_lamports as u128;
    let sat = |v: u128| v.min(u64::MAX as u128) as u64;
    PaperCost {
        fee_lamports: sat(fee),
        entry_impact_lamports: sat(entry_impact),
        exit_impact_lamports: sat(exit_impact),
        latency_lamports: sat(latency),
        network_lamports: sat(network),
        total_lamports: sat(fee + entry_impact + exit_impact + latency + network),
    }
}

/// Network cost of one swap transaction in lamports: the base signature fee
/// plus the priority fee (`cu_price` micro-lamports per CU × CU limit).
pub fn tx_network_cost_lamports(cu_limit: u32, cu_price_micro_lamports: u64) -> u64 {
    const BASE_FEE_LAMPORTS: u64 = 5_000;
    let priority = cu_limit as u128 * cu_price_micro_lamports as u128 / 1_000_000;
    (BASE_FEE_LAMPORTS as u128 + priority).min(u64::MAX as u128) as u64
}

/// Simulate `tx` (a swap built WITHOUT unwrapping the output and with
/// `minimum_amount_out = 1`) and return how much `output_account` gained.
/// `pre_balance` is the output account's balance before the swap (0 when it
/// does not exist yet). Fail-closed: any simulation error, a missing account
/// in the simulated state or a non-positive delta is an error.
pub fn simulate_swap_output(
    rpc: &RpcClient,
    tx: &Transaction,
    output_account: &Pubkey,
    pre_balance: u64,
) -> Result<SimQuote, String> {
    let cfg = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        accounts: Some(RpcSimulateTransactionAccountsConfig {
            encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
            addresses: vec![output_account.to_string()],
        }),
        ..Default::default()
    };
    let resp = tokio::task::block_in_place(|| {
        rpc.simulate_transaction_with_config(tx, cfg)
            .map_err(|e| format!("quote simulation failed: {e}"))
    })?;
    let v = resp.value;
    if let Some(err) = v.err {
        let tail: Vec<String> = v
            .logs
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(3)
            .collect();
        return Err(format!("quote simulation rejected: {err:?} {tail:?}"));
    }
    let account = v
        .accounts
        .and_then(|a| a.into_iter().next())
        .flatten()
        .ok_or("quote simulation returned no output account state")?;
    let data = account
        .data
        .decode()
        .or_else(|| match &account.data {
            solana_account_decoder::UiAccountData::Binary(s, _) => {
                base64::engine::general_purpose::STANDARD.decode(s).ok()
            }
            _ => None,
        })
        .ok_or("quote simulation returned undecodable output account data")?;
    let post = token_amount(&data).ok_or("output account is not an SPL token account")?;
    let amount_out = post
        .checked_sub(pre_balance)
        .filter(|d| *d > 0)
        .ok_or("quote simulation delivered no output tokens")?;
    Ok(SimQuote {
        amount_out,
        compute_units: v.units_consumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_out_is_integer_and_never_zero() {
        assert_eq!(min_out(1_000_000, 100), 990_000);
        assert_eq!(min_out(1_000_001, 100), 990_000); // rounds down
        assert_eq!(
            min_out(u64::MAX, 50),
            (u64::MAX as u128 * 9_950 / 10_000) as u64
        );
        assert_eq!(min_out(10, 10_000), 1);
        assert_eq!(min_out(0, 100), 1);
        assert_eq!(min_out(5, 20_000), 1, "slippage above 100% is clamped");
    }

    #[test]
    fn price_impact_against_spot() {
        assert_eq!(price_impact_bps(1_000, 1_000), 0);
        assert_eq!(price_impact_bps(1_000, 1_100), 0);
        assert_eq!(price_impact_bps(1_000, 970), 300);
        assert_eq!(price_impact_bps(1_000, 0), 10_000);
        assert_eq!(price_impact_bps(0, 5), 10_000);
        assert_eq!(price_impact_bps(u64::MAX, u64::MAX / 2), 5_000);
    }

    fn inputs() -> EdgeInputs {
        EdgeInputs {
            take_profit_bps: 1_000,
            fee_rate_ppm: 2_500,
            entry_impact_bps: 100,
            exit_impact_pct: 100,
            network_cost_per_tx_lamports: 13_000,
            failed_tx_reserve: 0,
            exit_slippage_bps: 0,
            position_lamports: 10_000_000,
        }
    }

    #[test]
    fn edge_subtracts_both_legs_separately() {
        let cost = tx_network_cost_lamports(300_000, 26_666);
        assert_eq!(cost, 5_000 + 7_999);
        // 10% target, 0.25% fee per leg, 1% impact per leg, 13 000 lamports
        // per tx on 0.01 SOL (13 bps per tx).
        let b = edge_breakdown(&inputs()).unwrap();
        assert_eq!(b.fee_cost_bps, 50);
        assert_eq!(b.network_cost_bps, 26);
        assert_eq!(b.exit_cost_bps, 25 + 100 + 13);
        assert_eq!(b.net_edge_bps, 1_000 - 50 - 200 - 26);
        assert_eq!(b.worst_case_edge_bps, b.net_edge_bps);
    }

    #[test]
    fn exit_is_costed_conservatively_and_slippage_separately() {
        let b = edge_breakdown(&EdgeInputs {
            exit_impact_pct: 200,
            failed_tx_reserve: 1,
            exit_slippage_bps: 100,
            ..inputs()
        })
        .unwrap();
        assert_eq!(b.exit_impact_bps, 200);
        assert_eq!(b.failed_tx_reserve_bps, 13);
        assert_eq!(b.net_edge_bps, 1_000 - 50 - 100 - 200 - 26 - 13);
        assert_eq!(b.worst_case_edge_bps, b.net_edge_bps - 100);
        // An exit multiplier below 100% is never applied.
        let low = edge_breakdown(&EdgeInputs {
            exit_impact_pct: 10,
            ..inputs()
        })
        .unwrap();
        assert_eq!(low.exit_impact_bps, 100);
        // Impact alone can erase the target.
        let big = edge_breakdown(&EdgeInputs {
            entry_impact_bps: 600,
            ..inputs()
        })
        .unwrap();
        assert!(big.net_edge_bps < 0);
    }

    #[test]
    fn edge_never_overflows_and_zero_size_never_trades() {
        assert_eq!(
            edge_breakdown(&EdgeInputs {
                position_lamports: 0,
                ..inputs()
            }),
            None
        );
        let b = edge_breakdown(&EdgeInputs {
            take_profit_bps: 0,
            fee_rate_ppm: u64::MAX,
            entry_impact_bps: u64::MAX,
            exit_impact_pct: u64::MAX,
            network_cost_per_tx_lamports: u64::MAX,
            failed_tx_reserve: u64::MAX,
            exit_slippage_bps: u64::MAX,
            position_lamports: 1,
        })
        .unwrap();
        assert!(b.net_edge_bps < 0 && b.worst_case_edge_bps < 0);
        assert!(tx_network_cost_lamports(u32::MAX, u64::MAX) > 0);
    }

    #[test]
    fn position_liquidity_ratio() {
        assert_eq!(position_liquidity_bps(50_000_000, 20_000_000_000), Some(25));
        assert_eq!(position_liquidity_bps(1, 0), None);
        assert_eq!(position_liquidity_bps(u64::MAX, 1), Some(u64::MAX));
    }

    #[test]
    fn paper_cost_is_pessimistic_and_bounded() {
        let m = PaperCostModel {
            fee_rate_ppm: 2_500,
            exit_impact_pct: 200,
            latency_bps_per_leg: 50,
            network_cost_per_tx_lamports: 10_000,
        };
        // 0.05 SOL against 20 SOL on both legs.
        let c = paper_round_trip_cost(&m, 50_000_000, 20_000_000_000, 20_000_000_000);
        assert_eq!(c.fee_lamports, 250_000);
        assert_eq!(c.entry_impact_lamports, 124_688);
        assert_eq!(c.exit_impact_lamports, 2 * 124_688);
        assert_eq!(c.latency_lamports, 500_000);
        assert_eq!(c.network_lamports, 20_000);
        assert_eq!(
            c.total_lamports,
            250_000 + 124_688 + 249_376 + 500_000 + 20_000
        );
        // Liquidity gone at exit: the exit costs the whole position (x2).
        let gone = paper_round_trip_cost(&m, 50_000_000, 20_000_000_000, 0);
        assert_eq!(gone.exit_impact_lamports, 100_000_000);
        // Extreme inputs never overflow.
        let x = paper_round_trip_cost(
            &PaperCostModel {
                fee_rate_ppm: u64::MAX,
                exit_impact_pct: u64::MAX,
                latency_bps_per_leg: u64::MAX,
                network_cost_per_tx_lamports: u64::MAX,
            },
            u64::MAX,
            1,
            1,
        );
        assert_eq!(x.total_lamports, u64::MAX);
    }

    #[test]
    fn token_amount_layout() {
        let mut d = vec![0u8; 165];
        d[64..72].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(token_amount(&d), Some(42));
        assert_eq!(token_amount(&d[..70]), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn simulation_errors_and_missing_state_fail_closed() {
        let tx = Transaction::new_unsigned(solana_sdk::message::Message::new(&[], None));
        let acct = Pubkey::new_unique();
        // RPC failure.
        let rpc = RpcClient::new_mock("fails".to_string());
        assert!(simulate_swap_output(&rpc, &tx, &acct, 0).is_err());
        // A successful simulation that returns no output account state.
        let rpc = RpcClient::new_mock("succeeds".to_string());
        let e = simulate_swap_output(&rpc, &tx, &acct, 0).unwrap_err();
        assert!(e.contains("no output account"), "{e}");
    }
}
