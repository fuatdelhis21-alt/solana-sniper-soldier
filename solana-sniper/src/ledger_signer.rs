use crate::hw_signer::SignerAdapter;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

/// HwLedgerSigner: scaffold/placeholder for Ledger hardware signer adapter.
/// Replace the body of sign_transaction with actual device calls (HID/APDU) using a Ledger library or hidapi.
pub struct HwLedgerSigner {
    pub device_path: Option<String>,
}

impl HwLedgerSigner {
    pub fn new(device_path: Option<String>) -> Self {
        Self { device_path }
    }
}

impl SignerAdapter for HwLedgerSigner {
    fn sign_transaction(&self, _tx: &mut Transaction) -> Result<Signature, String> {
        // TODO: Implement Ledger signing flow:
        // - open HID transport to Ledger device (e.g., using hidapi)
        // - build Solana APDU payload or use appropriate Ledger app/transport
        // - request signature and convert to solana_sdk::signature::Signature
        // Until implemented, return an explicit error to indicate missing adapter.
        Err("HwLedgerSigner: not implemented. Implement Ledger HID/APDU calls or integrate a Ledger adapter crate.".to_string())
    }
}
