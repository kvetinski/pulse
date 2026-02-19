use std::sync::Arc;
use std::time::Duration;

use crate::application::steps::{CreateAccountStep, DeleteAccountStep, GetAccountStep};
use crate::domain::contracts::PartitionKeyStrategy;
use crate::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig, Step};

pub fn predefined_scenarios() -> Vec<Scenario> {
    let endpoint = default_endpoint();
    vec![
        Scenario::new(
            "CreateAndFetchAccountFast",
            vec![
                Arc::new(CreateAccountStep) as Arc<dyn Step>,
                Arc::new(GetAccountStep) as Arc<dyn Step>,
            ],
            ScenarioConfig {
                endpoint: endpoint.clone(),
                scenarios_per_sec: 10.0,
                max_concurrency: 50,
                duration: parse_duration_literal("30s").expect("valid duration literal"),
                repeat: RepeatPolicy::Every(
                    parse_duration_literal("1m").expect("valid duration literal"),
                ),
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        ),
        Scenario::new(
            "CreateAndFetchAccountOnce",
            vec![
                Arc::new(CreateAccountStep) as Arc<dyn Step>,
                Arc::new(GetAccountStep) as Arc<dyn Step>,
            ],
            ScenarioConfig {
                endpoint: endpoint.clone(),
                scenarios_per_sec: 3.0,
                max_concurrency: 10,
                duration: parse_duration_literal("20s").expect("valid duration literal"),
                repeat: RepeatPolicy::Once,
                partition_key_strategy: PartitionKeyStrategy::ScenarioId,
            },
        ),
        Scenario::new(
            "CreateGetDeleteAccount",
            vec![
                Arc::new(CreateAccountStep) as Arc<dyn Step>,
                Arc::new(GetAccountStep) as Arc<dyn Step>,
                Arc::new(DeleteAccountStep) as Arc<dyn Step>,
            ],
            ScenarioConfig {
                endpoint,
                scenarios_per_sec: 5.0,
                max_concurrency: 20,
                duration: parse_duration_literal("30s").expect("valid duration literal"),
                repeat: RepeatPolicy::Every(
                    parse_duration_literal("5m").expect("valid duration literal"),
                ),
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        ),
    ]
}

fn default_endpoint() -> String {
    std::env::var("PULSE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

fn parse_duration_literal(value: &str) -> Result<Duration, String> {
    if let Some(num) = value.strip_suffix('s') {
        let secs = num
            .parse::<u64>()
            .map_err(|_| format!("invalid seconds duration: {value}"))?;
        return Ok(Duration::from_secs(secs));
    }
    if let Some(num) = value.strip_suffix('m') {
        let mins = num
            .parse::<u64>()
            .map_err(|_| format!("invalid minutes duration: {value}"))?;
        return Ok(Duration::from_secs(mins * 60));
    }
    if let Some(num) = value.strip_suffix('h') {
        let hours = num
            .parse::<u64>()
            .map_err(|_| format!("invalid hours duration: {value}"))?;
        return Ok(Duration::from_secs(hours * 3600));
    }
    Err(format!(
        "unsupported duration format: {value} (use Ns/Nm/Nh)"
    ))
}
