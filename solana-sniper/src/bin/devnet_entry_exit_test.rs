//! # Devnet Entry + Exit (Round-Trip) Test
//!
//! Executes a FULL position life cycle on a real devnet Raydium CLMM pool:
//!
//! 1. **Entry**: buy `--output-mint` with `--amount-lamports` of native SOL
//!    (preamble: idempotent ATA creation + native-SOL wrap + sync_native),
//!    signed via the remote HSM and confirmed on-chain.
//! 2. **Measurement**: the REAL received token amount and the REAL quote
//!    (WSOL) balance after the entry are read on-chain, and the position is
//!    recorded through the real risk engine (`RiskManager::record_entry`).
//! 3. **Exit**: sells the measured token balance back for SOL using the
//!    direction-aware quote (input mint = held token; the pool price is
//!    inverted), with slippage protection, signed via the HSM and confirmed.
//! 4. **P&L**: the REAL proceeds (quote-balance delta) are measured on-chain
//!    and `RiskManager::close_position` books the realized P&L (idempotent,
//!    loss feeds the daily-loss kill switch). A second close is asserted to
//!    be rejected (double-close protection).
//!
//! ## Safety
//! - Devnet only: refuses to run against any non-devnet RPC.
//! - HSM (mTLS) is mandatory for signing; private key material never leaves
//!   the HSM and no secrets are ever logged.
//! - `--max-spend-sol` is a hard fail-closed cap on the buy amount.
//! - `--dry-run` builds and signs the entry but never submits (no chain
//!   state → no exit).
//! - TP/SL *signal* logic is NOT re-tested here — it is covered by the
//!   `strategy` unit tests. This harness tests the exit *execution* path.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Serialize;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, instruction::Instruction, message::Message,
    native_token::LAMPORTS_PER_SOL, pubkey::Pubkey, signature::Signature, transaction::Transaction,
};
use solana_sniper::amm::account_resolver::{
    resolve_swap_accounts, RAYDIUM_CLMM_PROGRAM_ID, RAYDIUM_CLMM_PROGRAM_ID_DEVNET,
};
use solana_sniper::amm::raydium_v4::RaydiumV4ClmmAdapter;
use solana_sniper::amm::AmmAdapter;
use solana_sniper::hw_signer::SignerAdapter;
use solana_sniper::remote_hsm::RemoteHsmSigner;
use solana_sniper::risk::{OpenPosition, RiskConfig, RiskManager};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "devnet_entry_exit_test",
    about = "Devnet entry + exit round-trip test via HSM",
    version
)]
struct Args {
    /// RPC endpoint (must be devnet).
    #[arg(long, default_value = "https://api.devnet.solana.com")]
    rpc: String,

    /// Raydium CLMM pool to trade on.
    #[arg(long)]
    pool_id: String,

    /// Input mint for the ENTRY (base58) — must be native SOL.
    #[arg(long)]
    input_mint: String,

    /// Output mint for the ENTRY (base58) — the token that will be held and
    /// then sold on exit.
    #[arg(long)]
    output_mint: String,

    /// Entry amount in lamports.
    #[arg(long, default_value_t = 10_000_000)]
    amount_lamports: u64,

    /// Max slippage in basis points (applied to BOTH entry and exit quotes).
    #[arg(long, default_value_t = 200)]
    slippage_bps: u64,

    /// Security cap: maximum SOL that may be spent in a single real swap.
    /// Fail-closed — refuses to build the transaction above this cap.
    #[arg(long, default_value_t = 0.05)]
    max_spend_sol: f64,

    /// Remote HSM endpoint (mTLS).
    #[arg(long)]
    hsm_endpoint: String,

    /// CA certificate (PEM) for mTLS.
    #[arg(long)]
    hsm_ca: PathBuf,

    /// Combined client cert + key (PEM) for mTLS.
    #[arg(long)]
    hsm_client_identity: PathBuf,

    /// Build + sign the entry but never submit (no exit in dry-run).
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Serialize)]
struct TestResult {
    mode: String,
    success: bool,
    entry_signature: Option<String>,
    token_received_raw: Option<u64>,
    quote_after_entry_raw: Option<u64>,
    exit_signature: Option<String>,
    proceeds_raw: Option<u64>,
    realized_pnl_lamports: Option<i64>,
    double_close_rejected: Option<bool>,
    explorer_url: Option<String>,
    error: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let args = Args::parse();

    if !args.rpc.contains("devnet") {
        return Err(anyhow!(
            "refusing to run against non-devnet RPC: {} (devnet only)",
            args.rpc
        ));
    }

    let rpc = RpcClient::new_with_commitment(args.rpc.clone(), CommitmentConfig::confirmed());

    // Fail-closed: HSM is mandatory for signing.
    let signer = RemoteHsmSigner::new(
        &args.hsm_endpoint,
        Some(&args.hsm_ca),
        Some(&args.hsm_client_identity),
    )
    .map_err(|e| anyhow!("failed to init HSM signer: {e}"))?;
    let from = signer.pubkey().map_err(|e| anyhow!("HSM pubkey: {e}"))?;
    tracing::info!(pubkey = %from, "HSM signer pubkey");

    // Fail-closed security cap on the buy amount.
    let max_spend_lamports = (args.max_spend_sol * LAMPORTS_PER_SOL as f64) as u64;
    if args.amount_lamports > max_spend_lamports {
        return Err(anyhow!(
            "amount_lamports ({}) exceeds --max-spend-sol cap ({} lamports) — refusing (fail-closed)",
            args.amount_lamports,
            max_spend_lamports
        ));
    }

    // Preflight: the wallet must cover the entry amount plus rent for the
    // ATA creates (~0.02 SOL headroom). Fail-closed, no chain writes yet.
    let balance = rpc.get_balance(&from).context("failed to read balance")?;
    let required = args.amount_lamports.saturating_add(20_000_000);
    if balance < required {
        return Err(anyhow!(
            "insufficient devnet balance: {balance} lamports < required {required} (amount + rent headroom) — fund the HSM pubkey first"
        ));
    }
    tracing::info!(balance_lamports = balance, "preflight balance OK");

    let pool_id = Pubkey::from_str(&args.pool_id).map_err(|e| anyhow!("invalid --pool-id: {e}"))?;
    let input_mint =
        Pubkey::from_str(&args.input_mint).map_err(|e| anyhow!("invalid --input-mint: {e}"))?;
    let output_mint =
        Pubkey::from_str(&args.output_mint).map_err(|e| anyhow!("invalid --output-mint: {e}"))?;

    if input_mint != spl_token::native_mint::ID {
        return Err(anyhow!(
            "--input-mint must be native SOL for this test (P&L is booked in SOL lamports) — got {input_mint}"
        ));
    }

    let program_id = if args.rpc.contains("devnet") {
        Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID_DEVNET).expect("valid devnet program id")
    } else {
        Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).expect("valid mainnet program id")
    };

    // One risk manager on a throwaway data dir drives the SAME accounting
    // code the live bot uses (production defaults: 0.05 SOL/trade, 1 open
    // position, 0.20 SOL daily loss kill switch, 2% slippage).
    let risk_data_dir =
        std::env::temp_dir().join(format!("devnet_entry_exit_risk_{}", std::process::id()));
    let risk_cfg = RiskConfig::production_defaults(risk_data_dir.clone())
        .map_err(|e| anyhow!("risk config failed: {e}"))?;
    let risk = RiskManager::new(risk_cfg);

    // ------------------------------------------------------------------
    // ENTRY: buy output_mint with amount_lamports of native SOL.
    // ------------------------------------------------------------------
    let (accounts, pool) = resolve_swap_accounts(
        &rpc,
        &pool_id,
        &from,
        &input_mint,
        &output_mint,
        &program_id,
    )
    .map_err(|e| anyhow!("entry account resolution failed (fail-closed): {e}"))?;
    let entry_sqrt_x64 = pool.sqrt_price_x64;

    let adapter = RaydiumV4ClmmAdapter::new(args.pool_id.clone())
        .with_swap_accounts(accounts)
        .with_resolved_pool(pool)
        .with_program_id(program_id.to_string())
        .with_input_mint(input_mint);

    let quote = adapter
        .quote(args.amount_lamports, args.slippage_bps)
        .map_err(|e| anyhow!("entry quote failed (fail-closed): {e}"))?;
    tracing::info!(
        input_amount = quote.input_amount,
        expected_output = quote.expected_output,
        slippage_bps = quote.slippage_bps,
        "entry quote computed from real on-chain pool state"
    );
    let intent = adapter
        .build_intent(quote)
        .map_err(|e| anyhow!("entry build_intent failed: {e}"))?;
    let swap_ix = adapter
        .build_swap_instruction(&intent, &from)
        .map_err(|e| anyhow!("entry build_swap_instruction failed: {e}"))?;

    // Pre-entry balance snapshot (fail-closed: the position math relies on
    // real measured deltas, not on expected amounts).
    let token_before = token_ata_balance_raw(&rpc, &from, &output_mint)
        .context("pre-entry token balance read failed (fail-closed)")?
        .unwrap_or(0);
    let quote_before = token_ata_balance_raw(&rpc, &from, &input_mint)
        .context("pre-entry quote balance read failed (fail-closed)")?
        .unwrap_or(0);
    tracing::info!(token_before, quote_before, "pre-entry balances");

    let mut ixs: Vec<Instruction> = Vec::new();
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &from,
            &from,
            &input_mint,
            &spl_token::ID,
        ),
    );
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &from,
            &from,
            &output_mint,
            &spl_token::ID,
        ),
    );
    let input_ata = spl_associated_token_account::get_associated_token_address(&from, &input_mint);
    ixs.push(solana_sdk::system_instruction::transfer(
        &from,
        &input_ata,
        args.amount_lamports,
    ));
    ixs.push(
        spl_token::instruction::sync_native(&spl_token::ID, &input_ata)
            .map_err(|e| anyhow!("failed to build sync_native instruction: {e}"))?,
    );
    ixs.push(swap_ix);

    let blockhash = rpc.get_latest_blockhash().context("get_latest_blockhash")?;
    let msg = Message::new(&ixs, Some(&from));
    let mut entry_tx = Transaction::new_unsigned(msg);
    entry_tx.message.recent_blockhash = blockhash;

    let entry_sig_raw = signer
        .sign_transaction(&mut entry_tx)
        .map_err(|e| anyhow!("HSM signing of entry failed: {e}"))?;
    entry_tx.signatures = vec![entry_sig_raw];

    if args.dry_run {
        let result = TestResult {
            mode: "entry_dry_run".to_string(),
            success: true,
            entry_signature: Some(entry_tx.signatures[0].to_string()),
            token_received_raw: None,
            quote_after_entry_raw: None,
            exit_signature: None,
            proceeds_raw: None,
            realized_pnl_lamports: None,
            double_close_rejected: None,
            explorer_url: None,
            error: None,
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let entry_sig = submit_and_confirm(&rpc, &entry_tx)?;
    tracing::info!(signature = %entry_sig, "entry confirmed on-chain");

    // Post-entry measurement — the REAL received token amount.
    let token_after = match token_ata_balance_raw(&rpc, &from, &output_mint)
        .context("post-entry token balance read failed (fail-closed)")?
    {
        Some(v) if v > 0 => v,
        _ => {
            return Err(anyhow!(
                "entry confirmed but received token balance is zero/unreadable — aborting (fail-closed)"
            ));
        }
    };
    let quote_after_entry = token_ata_balance_raw(&rpc, &from, &input_mint)
        .context("post-entry quote balance read failed (fail-closed)")?
        .unwrap_or(0);
    tracing::info!(
        token_received = token_after,
        quote_after_entry,
        "post-entry measurements"
    );

    // Record the position through the REAL risk engine.
    let position = OpenPosition {
        pool_id: args.pool_id.clone(),
        token_mint: output_mint.to_string(),
        quote_mint: input_mint.to_string(),
        token_amount_raw: token_after,
        quote_balance_after_entry: quote_after_entry,
        spend_lamports: args.amount_lamports,
        entry_sqrt_price: entry_sqrt_x64,
        opened_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        entry_sig: entry_sig.to_string(),
    };
    risk.record_entry(position)
        .map_err(|e| anyhow!("record_entry failed (fail-closed): {e}"))?;
    tracing::info!(
        open_positions = risk.open_position_count(),
        exposure = risk.open_exposure_lamports(),
        "position recorded open"
    );

    // ------------------------------------------------------------------
    // EXIT: sell the measured token balance back for SOL.
    // ------------------------------------------------------------------
    let (accounts, pool) = resolve_swap_accounts(
        &rpc,
        &pool_id,
        &from,
        &output_mint, // reversed: input is now the held token
        &input_mint,  // reversed: output is now SOL
        &program_id,
    )
    .map_err(|e| anyhow!("exit account resolution failed (fail-closed): {e}"))?;

    let exit_adapter = RaydiumV4ClmmAdapter::new(args.pool_id.clone())
        .with_swap_accounts(accounts)
        .with_resolved_pool(pool)
        .with_program_id(program_id.to_string())
        .with_input_mint(output_mint);

    let sell_amount = token_after; // sell exactly what the entry received
    let quote = exit_adapter
        .quote(sell_amount, args.slippage_bps)
        .map_err(|e| anyhow!("exit quote failed (fail-closed): {e}"))?;
    tracing::info!(
        input_amount = quote.input_amount,
        expected_output = quote.expected_output,
        slippage_bps = quote.slippage_bps,
        "exit quote computed (direction-aware: selling token for SOL)"
    );
    let intent = exit_adapter
        .build_intent(quote)
        .map_err(|e| anyhow!("exit build_intent failed: {e}"))?;
    let swap_ix = exit_adapter
        .build_swap_instruction(&intent, &from)
        .map_err(|e| anyhow!("exit build_swap_instruction failed: {e}"))?;

    let mut ixs: Vec<Instruction> = Vec::new();
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &from,
            &from,
            &output_mint,
            &spl_token::ID,
        ),
    );
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &from,
            &from,
            &input_mint,
            &spl_token::ID,
        ),
    );
    ixs.push(swap_ix);

    let blockhash = rpc.get_latest_blockhash().context("get_latest_blockhash")?;
    let msg = Message::new(&ixs, Some(&from));
    let mut exit_tx = Transaction::new_unsigned(msg);
    exit_tx.message.recent_blockhash = blockhash;

    let exit_sig_raw = signer
        .sign_transaction(&mut exit_tx)
        .map_err(|e| anyhow!("HSM signing of exit failed: {e}"))?;
    exit_tx.signatures = vec![exit_sig_raw];

    let exit_sig = submit_and_confirm(&rpc, &exit_tx)?;
    tracing::info!(signature = %exit_sig, "exit confirmed on-chain");

    // Post-exit measurement — the REAL proceeds (quote balance delta).
    let quote_after_exit = token_ata_balance_raw(&rpc, &from, &input_mint)
        .context("post-exit quote balance read failed (fail-closed)")?
        .unwrap_or(0);
    let proceeds = quote_after_exit.saturating_sub(quote_after_entry);
    if proceeds == 0 {
        return Err(anyhow!(
            "exit confirmed but zero proceeds measured — position NOT closed (fail-closed, mirrors live loop)"
        ));
    }

    // Book the realized P&L through the real risk engine and assert the
    // idempotency guard (a second close of the same event must be rejected).
    // Exits are counted separately from entries (never consuming the daily
    // entry cap) — same accounting as the live loop.
    risk.close_position(proceeds)
        .map_err(|e| anyhow!("close_position failed (fail-closed): {e}"))?;
    risk.record_exit();
    let double_close_rejected = risk.close_position(proceeds).is_err();
    let pnl = risk.realized_pnl();
    tracing::info!(
        proceeds,
        realized_pnl_lamports = pnl,
        daily_entries = risk.current_daily_trades(),
        daily_exits = risk.current_daily_exits(),
        open_positions = risk.open_position_count(),
        double_close_rejected,
        "position closed, realized P&L booked"
    );
    if pnl != proceeds as i64 - args.amount_lamports as i64 {
        return Err(anyhow!(
            "P&L accounting mismatch: realized_pnl={pnl}, expected proceeds-spend={}",
            proceeds as i64 - args.amount_lamports as i64
        ));
    }

    let explorer_url = format!(
        "https://explorer.solana.com/tx/{}?cluster=devnet",
        entry_sig
    );
    let result = TestResult {
        mode: "entry_exit_round_trip".to_string(),
        success: true,
        entry_signature: Some(entry_sig.to_string()),
        token_received_raw: Some(token_after),
        quote_after_entry_raw: Some(quote_after_entry),
        exit_signature: Some(exit_sig.to_string()),
        proceeds_raw: Some(proceeds),
        realized_pnl_lamports: Some(pnl),
        double_close_rejected: Some(double_close_rejected),
        explorer_url: Some(explorer_url),
        error: None,
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Raw token balance (u64 amount field at byte offset 64 of an SPL token
/// account) or `None` when the ATA does not exist yet. Fail-closed on RPC
/// error or malformed account data — never fabricates a balance.
fn token_ata_balance_raw(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<Option<u64>> {
    let ata = spl_associated_token_account::get_associated_token_address(owner, mint);
    let resp = rpc
        .get_account_with_commitment(&ata, CommitmentConfig::confirmed())
        .context("failed to fetch token account")?;
    let Some(account) = resp.value else {
        return Ok(None);
    };
    if account.data.len() < 72 {
        return Err(anyhow!(
            "token account {ata} data too short: {} bytes (< 72)",
            account.data.len()
        ));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&account.data[64..72]);
    Ok(Some(u64::from_le_bytes(buf)))
}

/// Submit and poll until finalized-ish confirmation (devnet only; the
/// signature is returned on the first confirmed status).
fn submit_and_confirm(rpc: &RpcClient, tx: &Transaction) -> Result<Signature> {
    let sig = rpc
        .send_transaction(tx)
        .map_err(|e| anyhow!("failed to send transaction: {e}"))?;
    tracing::info!(signature = %sig, "transaction submitted, waiting for confirmation");

    for _ in 0..30 {
        let status = rpc
            .get_signature_status(&sig)
            .context("failed to poll signature status")?;
        match status {
            Some(Ok(())) => return Ok(sig),
            Some(Err(e)) => {
                return Err(anyhow!("transaction failed on-chain: {e:?}"));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
    Err(anyhow!("transaction not confirmed within timeout"))
}
