//! # Devnet Swap Test
//!
//! Builds a real Raydium CLMM swap transaction using on-chain account
//! resolution, signs it via the remote HSM (mTLS), submits it to devnet, and
//! verifies the result on-chain.
//!
//! ## Modes
//! - **Real swap** (`--pool-id` + `--input-mint` + `--output-mint`): resolves
//!   the pool state and full swap account set, builds a real CLMM swap.
//! - **Mock DEX** (`--mock`): builds a self-contained mock DEX instruction to
//!   exercise the HSM sign → submit → confirm pipeline without a real pool.
//!
//! ## Safety
//! - Devnet only by default; `--rpc` must point at a devnet endpoint.
//! - No secrets are logged; only public keys, signatures and explorer URLs.
//! - `--dry-run` builds and signs but never submits.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Serialize;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, message::Message, pubkey::Pubkey, signature::Signature,
    transaction::Transaction,
};
use solana_sniper::amm::account_resolver::{
    resolve_swap_accounts, RAYDIUM_CLMM_PROGRAM_ID, RAYDIUM_CLMM_PROGRAM_ID_DEVNET,
};
use solana_sniper::amm::raydium_v4::RaydiumV4ClmmAdapter;
use solana_sniper::amm::AmmAdapter;
use solana_sniper::hw_signer::SignerAdapter;
use solana_sniper::remote_hsm::RemoteHsmSigner;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "devnet_swap_test",
    about = "Devnet AMM swap test via HSM",
    version
)]
struct Args {
    /// RPC endpoint (must be devnet for a real swap test).
    #[arg(long, default_value = "https://api.devnet.solana.com")]
    rpc: String,

    /// Raydium CLMM pool to trade on.
    #[arg(long)]
    pool_id: Option<String>,

    /// Input token mint (base58).
    #[arg(long)]
    input_mint: Option<String>,

    /// Output token mint (base58).
    #[arg(long)]
    output_mint: Option<String>,

    /// Input amount in lamports.
    #[arg(long, default_value_t = 1_000_000)]
    amount_lamports: u64,

    /// Max slippage in basis points.
    #[arg(long, default_value_t = 100)]
    slippage_bps: u64,

    /// Remote HSM endpoint (mTLS).
    #[arg(long)]
    hsm_endpoint: String,

    /// CA certificate (PEM) for mTLS.
    #[arg(long)]
    hsm_ca: PathBuf,

    /// Combined client cert + key (PEM) for mTLS.
    #[arg(long)]
    hsm_client_identity: PathBuf,

    /// Use a mock DEX instruction instead of a real pool swap.
    #[arg(long, default_value_t = false)]
    mock: bool,

    /// Build + sign but never submit.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Serialize)]
struct TestResult {
    mode: String,
    success: bool,
    signature: Option<String>,
    error: Option<String>,
    explorer_url: Option<String>,
    dry_run: bool,
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
            "refusing to run swap test against non-devnet RPC: {} (devnet only)",
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

    let blockhash = rpc
        .get_latest_blockhash()
        .context("failed to get recent blockhash")?;

    let mode;
    let mut tx = if args.mock {
        mode = "mock_dex".to_string();
        // Mock DEX instruction: a self-contained transfer that exercises the
        // HSM sign → submit → confirm pipeline without a real pool.
        let ix = solana_sdk::system_instruction::transfer(&from, &from, 1_000);
        let msg = Message::new(&[ix], Some(&from));
        let mut t = Transaction::new_unsigned(msg);
        t.message.recent_blockhash = blockhash;
        t
    } else {
        let pool_id_str = args
            .pool_id
            .as_ref()
            .ok_or_else(|| anyhow!("--pool-id is required unless --mock"))?;
        let input_mint_str = args
            .input_mint
            .as_ref()
            .ok_or_else(|| anyhow!("--input-mint is required unless --mock"))?;
        let output_mint_str = args
            .output_mint
            .as_ref()
            .ok_or_else(|| anyhow!("--output-mint is required unless --mock"))?;

        let pool_id =
            Pubkey::from_str(pool_id_str).map_err(|e| anyhow!("invalid --pool-id: {e}"))?;
        let input_mint =
            Pubkey::from_str(input_mint_str).map_err(|e| anyhow!("invalid --input-mint: {e}"))?;
        let output_mint =
            Pubkey::from_str(output_mint_str).map_err(|e| anyhow!("invalid --output-mint: {e}"))?;

        let program_id = if args.rpc.contains("devnet") {
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID_DEVNET).expect("valid devnet program id")
        } else {
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).expect("valid mainnet program id")
        };

        mode = "real_swap".to_string();

        // Account resolution: pool state + full swap account set.
        let (accounts, pool) = resolve_swap_accounts(
            &rpc,
            &pool_id,
            &from,
            &input_mint,
            &output_mint,
            &program_id,
        )
        .map_err(|e| anyhow!("account resolution failed (fail-closed): {e}"))?;

        tracing::info!(
            pool_id = %pool_id,
            amm_config = %accounts.amm_config,
            input_vault = %accounts.input_vault,
            output_vault = %accounts.output_vault,
            observation = %accounts.observation_state,
            tick_array = %accounts.tick_array,
            sqrt_price = pool.sqrt_price_x64,
            liquidity = pool.liquidity,
            "resolved pool state and swap accounts"
        );

        let adapter = RaydiumV4ClmmAdapter::new(pool_id_str.clone())
            .with_swap_accounts(accounts)
            .with_resolved_pool(pool);

        let quote = adapter
            .quote(args.amount_lamports, args.slippage_bps)
            .map_err(|e| anyhow!("quote failed: {e}"))?;
        let intent = adapter
            .build_intent(quote)
            .map_err(|e| anyhow!("build_intent failed: {e}"))?;
        let mut t = adapter
            .build_transaction(&intent, &from, blockhash)
            .map_err(|e| anyhow!("build_transaction failed: {e}"))?;
        t.message.recent_blockhash = blockhash;
        t
    };

    // Sign via HSM (mTLS). Private key material never leaves the HSM.
    let sig = signer
        .sign_transaction(&mut tx)
        .map_err(|e| anyhow!("HSM signing failed: {e}"))?;
    tx.signatures = vec![sig];

    tracing::info!(signature = %sig, "transaction signed via HSM (mTLS)");

    if args.dry_run {
        let result = TestResult {
            mode,
            success: true,
            signature: Some(sig.to_string()),
            error: None,
            explorer_url: None,
            dry_run: true,
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Submit and confirm on-chain.
    match submit_and_confirm(&rpc, &tx) {
        Ok(sig) => {
            let sig_str = sig.to_string();
            let explorer_url = format!("https://explorer.solana.com/tx/{}?cluster=devnet", sig_str);
            tracing::info!(signature = %sig_str, "transaction confirmed on-chain");
            let result = TestResult {
                mode,
                success: true,
                signature: Some(sig_str.clone()),
                error: None,
                explorer_url: Some(explorer_url),
                dry_run: false,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Err(e) => {
            let result = TestResult {
                mode,
                success: false,
                signature: None,
                error: Some(format!("{e}")),
                explorer_url: None,
                dry_run: false,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            Err(anyhow!("transaction failed: {e}"))
        }
    }
}

/// Submit a transaction and poll for on-chain confirmation, returning the
/// confirmed signature. Fail-closed: any error propagates.
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
