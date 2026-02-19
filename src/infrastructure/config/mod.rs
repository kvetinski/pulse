use std::time::Duration;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub kafka_brokers: String,
    pub kafka_jobs_topic: String,
    pub kafka_results_topic: String,
    pub kafka_group_id: String,
    pub redis_url: String,
    pub redis_leader_key: String,
    pub redis_schedule_prefix: String,
    pub redis_idempotency_prefix: String,
    pub node_id: String,
    pub leader_lock_ttl_ms: u64,
    pub leader_renew_interval: Duration,
    pub scheduler_tick_interval: Duration,
    pub queue_capacity: usize,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self {
            kafka_brokers: env_or("PULSE_KAFKA_BROKERS", "localhost:9092"),
            kafka_jobs_topic: env_or("PULSE_KAFKA_JOBS_TOPIC", "pulse.scenario.jobs"),
            kafka_results_topic: env_or("PULSE_KAFKA_RESULTS_TOPIC", "pulse.scenario.results"),
            kafka_group_id: env_or("PULSE_KAFKA_GROUP_ID", "pulse-workers"),
            redis_url: env_or("PULSE_REDIS_URL", "redis://127.0.0.1:6379"),
            redis_leader_key: env_or("PULSE_REDIS_LEADER_KEY", "pulse:leader"),
            redis_schedule_prefix: env_or("PULSE_REDIS_SCHEDULE_PREFIX", "pulse:schedule"),
            redis_idempotency_prefix: env_or("PULSE_REDIS_IDEMPOTENCY_PREFIX", "pulse:dedupe"),
            node_id: env_or("PULSE_NODE_ID", format!("node-{}", std::process::id())),
            leader_lock_ttl_ms: env_or_parse("PULSE_LEADER_LOCK_TTL_MS", 10_000_u64),
            leader_renew_interval: Duration::from_millis(env_or_parse(
                "PULSE_LEADER_RENEW_INTERVAL_MS",
                3_000_u64,
            )),
            scheduler_tick_interval: Duration::from_millis(env_or_parse(
                "PULSE_SCHEDULER_TICK_INTERVAL_MS",
                500_u64,
            )),
            queue_capacity: env_or_parse("PULSE_QUEUE_CAPACITY", 1024_usize),
        }
    }
}

fn env_or(name: &str, default: impl Into<String>) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}

fn env_or_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}
