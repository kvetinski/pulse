use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};
use tracing::info;

use crate::domain::error::PulseError;
use crate::domain::ports::{
    AccountGateway, CreateAccountInput, CreateAccountOutput, DeleteAccountInput, GetAccountInput,
    GetAccountOutput,
};
use crate::infrastructure::grpc::proto::account_service_client::AccountServiceClient;
use crate::infrastructure::grpc::proto::{
    CreateAccountRequest, DeleteAccountRequest, GetAccountRequest,
};

pub struct TonicAccountGateway {
    client: AccountServiceClient<Channel>,
}

impl TonicAccountGateway {
    pub async fn connect(endpoint: &str) -> Result<Self, PulseError> {
        info!(endpoint = %endpoint, "connecting gRPC gateway");
        let channel = Endpoint::from_shared(endpoint.to_string())
            .map_err(|e| PulseError::Client(format!("invalid endpoint: {e}")))?
            .connect()
            .await
            .map_err(|e| PulseError::Client(format!("connect failed: {e}")))?;

        info!(endpoint = %endpoint, "connected gRPC gateway");
        Ok(Self {
            client: AccountServiceClient::new(channel),
        })
    }
}

#[async_trait]
impl AccountGateway for TonicAccountGateway {
    async fn create_account(
        &self,
        input: CreateAccountInput,
    ) -> Result<CreateAccountOutput, PulseError> {
        let mut client = self.client.clone();
        let response = client
            .create_account(CreateAccountRequest { phone: input.phone })
            .await
            .map_err(|e| map_grpc_status("CreateAccount failed", e))?
            .into_inner();

        let account = response.account.ok_or_else(|| {
            PulseError::Client("CreateAccount returned empty account".to_string())
        })?;

        Ok(CreateAccountOutput { id: account.id })
    }

    async fn get_account(&self, input: GetAccountInput) -> Result<GetAccountOutput, PulseError> {
        let mut client = self.client.clone();
        let response = client
            .get_account(GetAccountRequest { id: input.id })
            .await
            .map_err(|e| map_grpc_status("GetAccount failed", e))?
            .into_inner();

        let account = response
            .account
            .ok_or_else(|| PulseError::Client("GetAccount returned empty account".to_string()))?;

        Ok(GetAccountOutput {
            id: account.id,
            phone: account.phone,
        })
    }

    async fn delete_account(&self, input: DeleteAccountInput) -> Result<(), PulseError> {
        let mut client = self.client.clone();
        client
            .delete_account(DeleteAccountRequest { id: input.id })
            .await
            .map_err(|e| map_grpc_status("DeleteAccount failed", e))?;
        Ok(())
    }
}

fn map_grpc_status(prefix: &str, status: tonic::Status) -> PulseError {
    PulseError::GrpcStatus {
        code: status.code().to_string(),
        message: format!("{prefix}: {status}"),
    }
}
