use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::context::ScenarioContext;
use crate::domain::contracts::PartitionKeyStrategy;
use crate::domain::error::PulseError;
use crate::domain::ports::{AccountGateway, PhoneGenerator};

#[derive(Clone)]
pub struct StepPorts {
    pub account_gateway: Arc<dyn AccountGateway>,
    pub phone_generator: Arc<dyn PhoneGenerator>,
}

#[async_trait]
pub trait Step: Send + Sync {
    fn name(&self) -> &'static str;
    async fn execute(&self, ctx: &mut ScenarioContext, ports: &StepPorts)
    -> Result<(), PulseError>;
}

#[derive(Clone)]
pub struct Scenario {
    pub name: String,
    pub steps: Vec<Arc<dyn Step>>,
    pub config: ScenarioConfig,
}

#[derive(Clone, Debug)]
pub struct ScenarioConfig {
    pub endpoint: String,
    pub scenarios_per_sec: f64,
    pub max_concurrency: usize,
    pub duration: Duration,
    pub repeat: RepeatPolicy,
    pub partition_key_strategy: PartitionKeyStrategy,
}

#[derive(Clone, Debug)]
pub enum RepeatPolicy {
    Once,
    Every(Duration),
}

impl Scenario {
    pub fn new(name: impl Into<String>, steps: Vec<Arc<dyn Step>>, config: ScenarioConfig) -> Self {
        Self {
            name: name.into(),
            steps,
            config,
        }
    }
}
