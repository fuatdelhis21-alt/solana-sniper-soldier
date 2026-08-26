use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Serialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use tracing_subscriber::EnvFilter;

// Compute Budget program ID (mainnet)
const COMPUTE_BUDGET_PROGRAM_ID: &str = "ComputeBudget111111111111111111111111111111";

/// Build a SetComputeUnitLimit instruction (discriminator 0x02)
fn set_compute_unit_limit_ix(units: u32) -> Instruction {
    let program_id = Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_ID).unwrap();
    let mut data = vec![2u8]; // discriminator
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![],
        data,
    }
}

/// Build a SetComputeUnitPrice instruction (discriminator 0x03), in micro-lamports
fn set_compute_unit_price_ix(micro_lamports: u64) -> Instruction {
    let program_id = Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_ID).unwrap();
    let mut data = vec![3u8]; // discriminator
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![],
        data,
    }
}

/// Solana HFT Platform — send_transfer
///
/// Ultra-low-latency SOL transfer with priority fee support.
#[derive(Parser, Debug)]
#[command(name = "send_transfer", about = "Send SOL with priority fees", version)]
struct Args {
    /// RPC endpoint URL
    #[arg(long, default_value = "https://api.devnet.solana.com")]
    rpc: String,

    /// Wallet keypair file (JSON)
    #[arg(long, default_value = "./wallet.json")]
    wallet: PathBuf,

    /// Destination public key
    #[arg(long)]
    to: String,

    /// Amount in SOL
    #[arg(long, default_value = "0.01")]
    amount: f64,

    /// Priority fee in micro-lamports per compute unit
    #[arg(long, default_value = "10000")]
    priority_fee_microlamports: u64,

    /// Compute units to request
    #[arg(long, default_value = "500")]
    compute_units: u32,

    /// Dry-run (simulate only)
    #[arg(long)]
    dry_run: bool,

    /// Jito tip in SOL (applied if >0)
    #[arg(long, default_value = "0.0")]
    jito_tip: f64,
}

#[derive(Serialize)]
struct TxResult {
    success: bool,
    signature: Option<String>,
    slot: Option<u64>,
    error: Option<String>,
    explorer_url: Option<String>,
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let args = Args::parse();

    // Load wallet
    let payer = read_keypair_file(&args.wallet).map_err(|e| {
        anyhow!(
            "Failed to read keypair from {}: {}",
            args.wallet.display(),
            e
        )
    })?;
    let payer_pubkey = payer.pubkey();

    // Parse destination
    let to_pubkey = Pubkey::from_str(&args.to)
        .map_err(|e| anyhow!("Invalid destination address '{}': {}", args.to, e))?;

    // Convert SOL to lamports
    let amount_lamports = (args.amount * 1_000_000_000.0) as u64;

    tracing::info!(
        from = %payer_pubkey,
        to = %to_pubkey,
        amount_lamports,
        rpc = %args.rpc,
        dry_run = args.dry_run,
        "Preparing transfer"
    );

    // Create RPC client
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        args.rpc.clone(),
        CommitmentConfig::processed(),
    ));

    // Build transfer instruction
    let transfer_ix =
        solana_sdk::system_instruction::transfer(&payer_pubkey, &to_pubkey, amount_lamports);

    // Build priority fee instruction (ComputeBudget program)
    let mut instructions: Vec<Instruction> = Vec::new();

    // Set compute unit price (priority fee) — raw instruction building
    instructions.push(set_compute_unit_price_ix(args.priority_fee_microlamports));

    // Set compute unit limit
    instructions.push(set_compute_unit_limit_ix(args.compute_units));

    // Add the actual transfer
    instructions.push(transfer_ix);

    // Get recent blockhash
    let blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .context("Failed to get recent blockhash")?;

    // Create and sign the message
    let message = Message::new_with_blockhash(&instructions, Some(&payer_pubkey), &blockhash);
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[&payer], blockhash);

    if args.dry_run {
        // Simulate the transaction
        match rpc_client.simulate_transaction(&tx).await {
            Ok(sim_result) => {
                let success = sim_result.value.err.is_none();
                let error = sim_result.value.err.map(|e| format!("{:?}", e));
                let logs = sim_result.value.logs.unwrap_or_default();

                tracing::info!(
                    simulation_success = success,
                    simulation_logs = ?logs,
                    "Simulation completed"
                );

                let result = TxResult {
                    success,
                    signature: None,
                    slot: None,
                    error,
                    explorer_url: None,
                    dry_run: true,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Err(e) => {
                tracing::error!(error = %e, "Simulation failed");
                let result = TxResult {
                    success: false,
                    signature: None,
                    slot: None,
                    error: Some(format!("{}", e)),
                    explorer_url: None,
                    dry_run: true,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
                std::process::exit(1);
            }
        }
    } else {
        // Send and confirm the transaction
        match rpc_client.send_and_confirm_transaction(&tx).await {
            Ok(sig) => {
                let sig_str = sig.to_string();
                let explorer_url = format!(
                    "https://explorer.solana.com/tx/{}?cluster={}",
                    sig_str,
                    if args.rpc.contains("devnet") {
                        "devnet"
                    } else if args.rpc.contains("testnet") {
                        "testnet"
                    } else {
                        "mainnet-beta"
                    }
                );

                tracing::info!(
                    signature = %sig_str,
                    explorer = %explorer_url,
                    "Transaction confirmed"
                );

                let result = TxResult {
                    success: true,
                    signature: Some(sig_str.clone()),
                    slot: None,
                    error: None,
                    explorer_url: Some(explorer_url),
                    dry_run: false,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Err(e) => {
                tracing::error!(error = %e, "Transaction failed");
                let result = TxResult {
                    success: false,
                    signature: None,
                    slot: None,
                    error: Some(format!("{}", e)),
                    explorer_url: None,
                    dry_run: false,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}
