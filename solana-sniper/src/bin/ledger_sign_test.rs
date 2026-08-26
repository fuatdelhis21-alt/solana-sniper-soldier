use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use solana_sniper::hw_signer::{HwSignerStub, LocalKeypairSigner, SignerAdapter};
use solana_sniper::ledger_signer::HwLedgerSigner;

fn main() {
    let keyfile = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./wallet.json".to_string());
    let device_path = std::env::args().nth(2);

    println!("=== Ledger Signer Test ===");
    println!("Keyfile: {}", keyfile);
    if let Some(ref dp) = device_path {
        println!("Device path: {}", dp);
    } else {
        println!("Device path: (default / auto-detect)");
    }

    // Build a dummy transfer tx message (1 lamport to a random address) for signing test
    let from = solana_sdk::signature::read_keypair_file(&keyfile).expect("read keypair");
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&from.pubkey(), &to, 1);
    let msg = Message::new(&[ix], Some(&from.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);

    // Try Ledger hardware signer first
    let ledger = HwLedgerSigner::new(device_path);
    match ledger.sign_transaction(&mut tx) {
        Ok(sig) => println!("[LEDGER] Signature: {}", sig),
        Err(e) => {
            println!("[LEDGER] Not available: {}", e);
            // Try generic HW stub
            let hw = HwSignerStub::new();
            match hw.sign_transaction(&mut tx) {
                Ok(sig2) => println!("[HW-STUB] Signature: {}", sig2),
                Err(e2) => {
                    println!("[HW-STUB] Not available: {}", e2);
                    // Fallback to local keyfile signer
                    let local = LocalKeypairSigner::new(&keyfile);
                    match local.sign_transaction(&mut tx) {
                        Ok(sig3) => println!("[LOCAL] Signature: {}", sig3),
                        Err(e3) => println!("[LOCAL] Failed: {}", e3),
                    }
                }
            }
        }
    }
}
