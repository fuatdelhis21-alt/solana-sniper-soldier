pub mod amm_adapter;
pub mod backend;
pub mod jito;
pub mod jito_bundle;
pub mod order;
pub mod raydium_v4;
pub mod rpc;

use std::collections::HashMap;

/// TransactionStore: emir kimliği → serialized tx eşlemesi.
#[derive(Debug, Default)]
pub struct TransactionStore(HashMap<u64, Vec<u8>>);

impl TransactionStore {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn register(&mut self, id: u64, data: Vec<u8>) {
        self.0.insert(id, data);
    }

    pub fn get(&self, id: u64) -> Option<&[u8]> {
        self.0.get(&id).map(|v| v.as_slice())
    }
}
