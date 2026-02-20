use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;

use crate::application::template::{
    extract_path, render_json_templates, render_string_template, value_to_context_string,
};
use crate::domain::context::ScenarioContext;
use crate::domain::error::PulseError;
use crate::domain::ports::DynamicGrpcRequest;
use crate::domain::scenario::{Step, StepPorts};

pub enum GrpcRequestSpec {
    Empty,
    StaticBase64Template(String),
    FieldTemplate(Value),
}

pub struct DynamicGrpcStep {
    name: String,
    endpoint_override: Option<String>,
    service: String,
    method: String,
    request_spec: GrpcRequestSpec,
    extraction: HashMap<String, String>,
    response_payload_context_key: Option<String>,
}

impl DynamicGrpcStep {
    pub fn new(
        endpoint_override: Option<String>,
        service: String,
        method: String,
        request_spec: GrpcRequestSpec,
        extraction: HashMap<String, String>,
        response_payload_context_key: Option<String>,
    ) -> Self {
        Self {
            name: format!("grpc:{service}/{method}"),
            endpoint_override,
            service,
            method,
            request_spec,
            extraction,
            response_payload_context_key,
        }
    }
}

#[async_trait]
impl Step for DynamicGrpcStep {
    fn name(&self) -> &str {
        &self.name
    }

    fn requires_dynamic_grpc(&self) -> bool {
        true
    }

    fn dynamic_grpc_endpoint_override(&self) -> Option<&str> {
        self.endpoint_override.as_deref()
    }

    async fn execute(
        &self,
        ctx: &mut ScenarioContext,
        ports: &StepPorts,
    ) -> Result<(), PulseError> {
        let endpoint = self
            .endpoint_override
            .as_deref()
            .unwrap_or(&ports.default_endpoint);
        let dynamic_gateway = ports.dynamic_grpc_gateways.get(endpoint).ok_or_else(|| {
            PulseError::Client(format!(
                "dynamic gRPC gateway is not configured for endpoint '{}' in step '{}'",
                endpoint, self.name
            ))
        })?;

        let payload = match &self.request_spec {
            GrpcRequestSpec::Empty => Vec::new(),
            GrpcRequestSpec::StaticBase64Template(template) => {
                let rendered = render_string_template(template, ctx)?;
                base64::engine::general_purpose::STANDARD
                    .decode(rendered.as_bytes())
                    .map_err(|e| {
                        PulseError::Client(format!(
                            "invalid rendered request_base64 for {}/{}: {e}",
                            self.service, self.method
                        ))
                    })?
            }
            GrpcRequestSpec::FieldTemplate(template) => {
                let rendered = render_json_templates(template, ctx)?;
                dynamic_gateway.encode_request_fields(&self.service, &self.method, &rendered)?
            }
        };

        let response = dynamic_gateway
            .unary(DynamicGrpcRequest {
                service: self.service.clone(),
                method: self.method.clone(),
                payload,
            })
            .await?;

        if let Some(key) = &self.response_payload_context_key {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&response.payload);
            ctx.set(key, encoded);
        }

        if !self.extraction.is_empty() {
            let decoded = dynamic_gateway.decode_response_fields(
                &self.service,
                &self.method,
                &response.payload,
            )?;
            for (ctx_key, response_path) in &self.extraction {
                let value = extract_path(&decoded, response_path).ok_or_else(|| {
                    PulseError::Client(format!(
                        "extraction path '{response_path}' not found in response for {}/{}",
                        self.service, self.method
                    ))
                })?;
                let serialized = value_to_context_string(value)?;
                ctx.set(ctx_key, serialized);
            }
        }

        Ok(())
    }
}
