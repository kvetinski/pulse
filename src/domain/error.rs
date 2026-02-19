use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum PulseError {
    MissingContextVar(String),
    Client(String),
    GrpcStatus { code: String, message: String },
}

impl Display for PulseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingContextVar(k) => write!(f, "missing context var: {k}"),
            Self::Client(m) => write!(f, "client error: {m}"),
            Self::GrpcStatus { code, message } => {
                write!(f, "grpc status error ({code}): {message}")
            }
        }
    }
}

impl std::error::Error for PulseError {}

impl PulseError {
    pub fn kind_label(&self) -> String {
        match self {
            Self::MissingContextVar(_) => "missing_context_var".to_string(),
            Self::Client(_) => "client_error".to_string(),
            Self::GrpcStatus { code, .. } => format!("grpc:{code}"),
        }
    }
}
