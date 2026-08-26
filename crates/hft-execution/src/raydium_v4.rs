use crate::amm_adapter::AmmAdapter;

pub struct RaydiumV4Adapter {
    pub program_id: String,
}

impl RaydiumV4Adapter {
    pub fn new(program_id: &str) -> Self {
        Self {
            program_id: program_id.to_string(),
        }
    }
}

impl AmmAdapter for RaydiumV4Adapter {
    fn init(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // placeholder: setup RPC / subscription
        println!(
            "RaydiumV4Adapter init (placeholder) for {}",
            self.program_id
        );
        Ok(())
    }

    fn get_pool_state(
        &self,
        pool_id: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // placeholder: return mock state
        Ok(format!("{{'pool_id':'{}','liquidity':12345}}", pool_id))
    }

    fn build_tx_plan(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(format!("tx_plan_for_{}", input))
    }
}
