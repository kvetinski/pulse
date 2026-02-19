use async_trait::async_trait;

use crate::domain::context::ScenarioContext;
use crate::domain::error::PulseError;
use crate::domain::ports::{CreateAccountInput, DeleteAccountInput, GetAccountInput};
use crate::domain::scenario::{Step, StepPorts};

pub struct CreateAccountStep;

#[async_trait]
impl Step for CreateAccountStep {
    fn name(&self) -> &'static str {
        "create_account"
    }

    async fn execute(
        &self,
        ctx: &mut ScenarioContext,
        ports: &StepPorts,
    ) -> Result<(), PulseError> {
        let phone = ports.phone_generator.generate();
        ctx.set("phone", phone.clone());

        let response = ports
            .account_gateway
            .create_account(CreateAccountInput { phone })
            .await?;
        ctx.set("user_id", response.id);
        Ok(())
    }
}

pub struct GetAccountStep;

#[async_trait]
impl Step for GetAccountStep {
    fn name(&self) -> &'static str {
        "get_account"
    }

    async fn execute(
        &self,
        ctx: &mut ScenarioContext,
        ports: &StepPorts,
    ) -> Result<(), PulseError> {
        let user_id = ctx
            .get("user_id")
            .ok_or_else(|| PulseError::MissingContextVar("user_id".to_string()))?
            .to_string();

        let response = ports
            .account_gateway
            .get_account(GetAccountInput { id: user_id })
            .await?;

        ctx.set("fetched_id", response.id);
        ctx.set("fetched_phone", response.phone);
        Ok(())
    }
}

pub struct DeleteAccountStep;

#[async_trait]
impl Step for DeleteAccountStep {
    fn name(&self) -> &'static str {
        "delete_account"
    }

    async fn execute(
        &self,
        ctx: &mut ScenarioContext,
        ports: &StepPorts,
    ) -> Result<(), PulseError> {
        let user_id = ctx
            .get("user_id")
            .ok_or_else(|| PulseError::MissingContextVar("user_id".to_string()))?
            .to_string();

        ports
            .account_gateway
            .delete_account(DeleteAccountInput { id: user_id })
            .await?;
        Ok(())
    }
}
