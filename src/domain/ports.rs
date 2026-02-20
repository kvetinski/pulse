use async_trait::async_trait;
use serde_json::Value;

use crate::domain::error::PulseError;

#[derive(Clone, Debug)]
pub struct DynamicGrpcRequest {
    pub service: String,
    pub method: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct DynamicGrpcResponse {
    pub payload: Vec<u8>,
}

#[async_trait]
pub trait DynamicGrpcGateway: Send + Sync {
    async fn unary(&self, input: DynamicGrpcRequest) -> Result<DynamicGrpcResponse, PulseError>;

    fn encode_request_fields(
        &self,
        service: &str,
        method: &str,
        fields: &Value,
    ) -> Result<Vec<u8>, PulseError>;

    fn decode_response_fields(
        &self,
        service: &str,
        method: &str,
        payload: &[u8],
    ) -> Result<Value, PulseError>;
}
