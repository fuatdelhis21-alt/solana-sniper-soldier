pub trait AmmAdapter {
    /// Initialize adapter (connect to RPC, subscribe, etc.)
    fn init(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Query pool state for a given pool id (placeholder return type)
    fn get_pool_state(
        &self,
        pool_id: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;

    /// Build a transaction plan for the action (placeholder)
    fn build_tx_plan(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
}
