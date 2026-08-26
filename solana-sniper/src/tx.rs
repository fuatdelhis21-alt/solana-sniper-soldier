//! Transaction building helpers — priority fee, compute budget, versioned transactions.

use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

/// Build a signed transaction with priority fee and compute unit limit.
pub fn build_signed_tx(
    signer: &Keypair,
    instructions: &[Instruction],
    blockhash: Hash,
    priority_microlamports: u64,
    compute_units: u32,
) -> Transaction {
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
    tx
}

/// Build a transfer transaction with priority fee.
pub fn build_transfer_tx(
    signer: &Keypair,
    to: &Pubkey,
    lamports: u64,
    blockhash: Hash,
) -> Transaction {
    let ix = solana_sdk::system_instruction::transfer(&signer.pubkey(), to, lamports);
    build_signed_tx(signer, &[ix], blockhash, 10_000, 200_000)
}
