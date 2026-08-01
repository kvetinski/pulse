use std::fmt::{Display, Formatter};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    UnsupportedVersion {
        found: u16,
        min_supported: u16,
        max_supported: u16,
    },
    FutureVersion {
        found: u16,
        max_supported: u16,
    },
    MissingField(&'static str),
    InvalidLoad(String),
    InvalidSlice(String),
    InvalidAttempt {
        attempt: u32,
        max_retries: u32,
    },
    InvalidResult(String),
    SafetyLimit(String),
    Malformed(String),
}

impl Display for ContractError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion {
                found,
                min_supported,
                max_supported,
            } => write!(
                f,
                "unsupported contract version {found}; supported range is {min_supported}..={max_supported}"
            ),
            Self::FutureVersion {
                found,
                max_supported,
            } => write!(
                f,
                "future contract version {found}; maximum supported is {max_supported}"
            ),
            Self::MissingField(field) => write!(f, "required field '{field}' is empty"),
            Self::InvalidLoad(message) => write!(f, "invalid load configuration: {message}"),
            Self::InvalidSlice(message) => write!(f, "invalid slice metadata: {message}"),
            Self::InvalidAttempt {
                attempt,
                max_retries,
            } => write!(
                f,
                "invalid attempt {attempt}; maximum retry attempt is {max_retries}"
            ),
            Self::InvalidResult(message) => write!(f, "invalid result: {message}"),
            Self::SafetyLimit(message) => write!(f, "safety limit exceeded: {message}"),
            Self::Malformed(message) => write!(f, "malformed contract: {message}"),
        }
    }
}

impl std::error::Error for ContractError {}

#[derive(Debug)]
pub enum PulseError {
    MissingContextVar(String),
    /// Legacy construction/encoding error. New code should prefer `InvalidScenario`.
    Client(String),
    /// Legacy target status variant retained for contract compatibility.
    GrpcStatus {
        code: String,
        message: String,
    },
    InvalidScenario(String),
    TargetTransport(String),
    TargetStatus {
        code: String,
        message: String,
    },
    RequestTimeout {
        step: String,
    },
    TransientInfrastructure(String),
    PermanentProcessing(String),
    Cancelled(String),
    InvariantViolation(String),
}

impl Display for PulseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingContextVar(k) => write!(f, "missing context var: {k}"),
            Self::Client(m) => write!(f, "client error: {m}"),
            Self::GrpcStatus { code, message } => {
                write!(f, "grpc status error ({code}): {message}")
            }
            Self::InvalidScenario(m) => write!(f, "invalid scenario: {m}"),
            Self::TargetTransport(m) => write!(f, "target transport error: {m}"),
            Self::TargetStatus { code, message } => write!(f, "target status ({code}): {message}"),
            Self::RequestTimeout { step } => write!(f, "request deadline exceeded for {step}"),
            Self::TransientInfrastructure(m) => write!(f, "transient infrastructure error: {m}"),
            Self::PermanentProcessing(m) => write!(f, "permanent processing error: {m}"),
            Self::Cancelled(m) => write!(f, "cancelled: {m}"),
            Self::InvariantViolation(m) => write!(f, "internal invariant violation: {m}"),
        }
    }
}

impl std::error::Error for PulseError {}

impl PulseError {
    pub fn kind_label(&self) -> String {
        match self {
            Self::MissingContextVar(_) => "missing_context_var".to_string(),
            Self::Client(_) => "invalid_scenario".to_string(),
            Self::GrpcStatus { code, .. } => format!("target_status:{code}"),
            Self::InvalidScenario(_) => "invalid_scenario".to_string(),
            Self::TargetTransport(_) => "target_transport".to_string(),
            Self::TargetStatus { code, .. } => format!("target_status:{code}"),
            Self::RequestTimeout { .. } => "target_timeout".to_string(),
            Self::TransientInfrastructure(_) => "pulse_infrastructure".to_string(),
            Self::PermanentProcessing(_) => "permanent_processing".to_string(),
            Self::Cancelled(_) => "cancelled".to_string(),
            Self::InvariantViolation(_) => "invariant_violation".to_string(),
        }
    }

    pub fn is_target_measurement(&self) -> bool {
        matches!(
            self,
            Self::GrpcStatus { .. }
                | Self::TargetTransport(_)
                | Self::TargetStatus { .. }
                | Self::RequestTimeout { .. }
        )
    }

    pub fn is_retryable_infrastructure(&self) -> bool {
        matches!(self, Self::TransientInfrastructure(_))
    }
}
