use solana_sdk::signature::{Signature, Signer};
use solana_sdk::transaction::Transaction;

pub trait SignerAdapter: Send + Sync {
    /// Sign raw tx bytes (serialized Transaction) and return signature
    fn sign_transaction(&self, tx: &mut Transaction) -> Result<Signature, String>;
}

/// LocalKeypairSigner: fallback signer which reads keypair file (wallet.json)
pub struct LocalKeypairSigner {
    pub keypair_path: String,
}

impl LocalKeypairSigner {
    pub fn new(path: &str) -> Self {
        Self {
            keypair_path: path.to_string(),
        }
    }
}

impl SignerAdapter for LocalKeypairSigner {
    fn sign_transaction(&self, tx: &mut Transaction) -> Result<Signature, String> {
        match solana_sdk::signature::read_keypair_file(&self.keypair_path) {
            Ok(kp) => {
                let msg = tx.message.serialize();
                let sig = kp
                    .try_sign_message(&msg)
                    .map_err(|e| format!("Sign error: {}", e))?;
                Ok(sig)
            }
            Err(e) => Err(format!("Failed to parse keypair: {}", e)),
        }
    }
}

/// HwSignerStub: placeholder for real hardware signer integration.
/// Implement actual Ledger/Trezor/HSM adapter that implements SignerAdapter.
pub struct HwSignerStub {}

impl HwSignerStub {
    pub fn new() -> Self {
        Self {}
    }
}

impl SignerAdapter for HwSignerStub {
    fn sign_transaction(&self, _tx: &mut Transaction) -> Result<Signature, String> {
        Err("HwSignerStub: no hardware signer attached".to_string())
    }
}
