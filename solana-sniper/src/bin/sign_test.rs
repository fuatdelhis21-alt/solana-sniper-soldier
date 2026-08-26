use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use solana_sniper::hw_signer::{HwSignerStub, LocalKeypairSigner, SignerAdapter};

fn main() {
    let keyfile = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./wallet.json".to_string());
    println!("Using keyfile: {}", keyfile);

    // Build a dummy transfer tx message (1 lamport to a random address) for signing test
    let from = solana_sdk::signature::read_keypair_file(&keyfile).expect("read keypair");
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&from.pubkey(), &to, 1);
    let msg = Message::new(&[ix], Some(&from.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);

    // Try hardware signer first (stub)
    let hw = HwSignerStub::new();
    match hw.sign_transaction(&mut tx) {
        Ok(sig) => println!("Hw signer produced signature: {}", sig),
        Err(e) => {
            println!(
                "Hw signer not available: {}. Falling back to local keyfile signer.",
                e
            );
            let local = LocalKeypairSigner::new(&keyfile);
            match local.sign_transaction(&mut tx) {
                Ok(sig2) => println!("Local signer signature: {}", sig2),
                Err(e2) => println!("Local signer failed: {}", e2),
            }
        }
    }
}
