//! Transaction executor — builds, signs, and sends transactions with retry + priority fee.

use std::sync::Arc;

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;
use tracing::{error, info, warn};

use crate::retry;

/// Execute a list of instructions as a single transaction with priority fee and retry.
pub fn execute_instructions(
    rpc: &RpcClient,
    signer: &Keypair,
    instructions: &[Instruction],
    blockhash: Hash,
    priority_microlamports: u64,
    compute_units: u32,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let from = signer.pubkey();

    let mut all_ixs = Vec::with_capacity(instructions.len() + 2);
    all_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
        priority_microlamports,
    ));
    all_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(
        compute_units,
    ));
    all_ixs.extend_from_slice(instructions);

    let message = Message::new(&all_ixs, Some(&from));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[signer], blockhash);

    info!(
        target: "executor",
        from = %from,
        num_ixs = all_ixs.len(),
        blockhash = %blockhash,
        "sending transaction"
    );

    retry::send_with_retry(rpc, &tx)
}

/// Execute a single (simplified) transfer — convenience wrapper.
pub fn execute_transfer(
    rpc: &RpcClient,
    signer: &Keypair,
    to: &solana_sdk::pubkey::Pubkey,
    lamports: u64,
    blockhash: Hash,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let ix = solana_sdk::system_instruction::transfer(&signer.pubkey(), to, lamports);
    execute_instructions(rpc, signer, &[ix], blockhash, 10_000, 200_000)
}
