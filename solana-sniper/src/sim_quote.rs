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

/// Expected net edge (bps of the position) of a round trip that reaches the
/// take-profit: the target gain minus the trading fee on both legs, the
/// price impact on both legs (the exit is assumed to cost what the entry
/// did) and the network / priority fees of both transactions. Negative when
/// the costs eat the target.
pub fn net_edge_bps(
    take_profit_bps: u64,
    fee_rate_ppm: u64,
    entry_impact_bps: u64,
    network_cost_lamports: u64,
    position_lamports: u64,
) -> i64 {
    let fee_bps = (fee_rate_ppm / 100) as i64; // ppm -> bps
    let network_bps = if position_lamports == 0 {
        i64::MAX / 4
    } else {
        (network_cost_lamports as u128 * 10_000 / position_lamports as u128) as i64
    };
    take_profit_bps as i64 - 2 * fee_bps - 2 * entry_impact_bps as i64 - 2 * network_bps
}

/// Network cost of one swap transaction in lamports: the base signature fee
/// plus the priority fee (`cu_price` micro-lamports per CU × CU limit).
pub fn tx_network_cost_lamports(cu_limit: u32, cu_price_micro_lamports: u64) -> u64 {
    const BASE_FEE_LAMPORTS: u64 = 5_000;
    BASE_FEE_LAMPORTS + (cu_limit as u128 * cu_price_micro_lamports as u128 / 1_000_000) as u64
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

    #[test]
    fn net_edge_subtracts_round_trip_costs() {
        // 10% target, 0.25% fee (2500 ppm), 1% impact, 13 000 lamports per
        // tx on a 0.01 SOL position (1.3%).
        let cost = tx_network_cost_lamports(300_000, 26_666);
        assert_eq!(cost, 5_000 + 7_999);
        let edge = net_edge_bps(1_000, 2_500, 100, 13_000, 10_000_000);
        assert_eq!(edge, 1_000 - 50 - 200 - 26);
        // Impact alone can erase the target.
        assert!(net_edge_bps(1_000, 2_500, 600, 13_000, 10_000_000) < 0);
        assert!(
            net_edge_bps(1_000, 0, 0, 1, 0) < 0,
            "zero size never trades"
        );
    }

    #[test]
    fn token_amount_layout() {
        let mut d = vec![0u8; 165];
        d[64..72].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(token_amount(&d), Some(42));
        assert_eq!(token_amount(&d[..70]), None);
    }
}
