use async_trait::async_trait;

use crate::domain::error::PulseError;

#[derive(Clone, Debug)]
pub struct CreateAccountInput {
    pub phone: String,
}

#[derive(Clone, Debug)]
pub struct CreateAccountOutput {
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct GetAccountInput {
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct GetAccountOutput {
    pub id: String,
    pub phone: String,
}

#[derive(Clone, Debug)]
pub struct DeleteAccountInput {
    pub id: String,
}

#[async_trait]
pub trait AccountGateway: Send + Sync {
    async fn create_account(
        &self,
        input: CreateAccountInput,
    ) -> Result<CreateAccountOutput, PulseError>;

    async fn get_account(&self, input: GetAccountInput) -> Result<GetAccountOutput, PulseError>;

    async fn delete_account(&self, input: DeleteAccountInput) -> Result<(), PulseError>;
}

pub trait PhoneGenerator: Send + Sync {
    fn generate(&self) -> String;
}
