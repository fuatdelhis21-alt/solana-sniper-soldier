use solana_sdk::signature::{Keypair, Signer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./wallet.json".to_string());
    let keypair = Keypair::new();
    let bytes = keypair.to_bytes().to_vec();
    std::fs::write(&path, serde_json::to_vec_pretty(&bytes)?)?;
    println!("wallet_path={path}");
    println!("pubkey={}", keypair.pubkey());
    Ok(())
}