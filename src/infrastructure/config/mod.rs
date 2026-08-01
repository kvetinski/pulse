use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;

use tonic::transport::Endpoint;

use crate::domain::contracts::MAX_POISON_EVIDENCE_FIELD_BYTES;

const MAX_KAFKA_POLL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_KAFKA_SESSION_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_CONFIGURED_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_QUEUE_CAPACITY: usize = 1_000_000;
const MAX_CONSUMER_QUEUE_KBYTES: usize = 1_048_576;
const MAX_KAFKA_MESSAGE_BYTES: usize = 256 * 1024 * 1024;
const POISON_ENVELOPE_OVERHEAD_BYTES: usize = 16 * 1024;
const MAX_TOPIC_PARTITIONS: i32 = 10_000;
const MAX_TOPIC_REPLICATION_FACTOR: i32 = 32;
const MAX_WORKER_RETRIES: u32 = 32;
const MAX_SAFETY_RATE: f64 = 1_000_000_000.0;
const MAX_SAFETY_CONCURRENCY: usize = 1_000_000;
const REDIS_OPERATION_TIMEOUT_CEILING: Duration = Duration::from_secs(2);
const KAFKA_POLL_SAFETY_MARGIN: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub kafka_brokers: String,
    pub kafka_jobs_topic: String,
    pub kafka_results_topic: String,
    pub kafka_summaries_topic: String,
    pub kafka_dlq_topic: String,
    pub kafka_group_id: String,
    pub kafka_aggregator_group_id: String,
    pub kafka_max_poll_interval: Duration,
    pub kafka_session_timeout: Duration,
    pub kafka_message_timeout: Duration,
    pub kafka_delivery_timeout: Duration,
    pub kafka_request_timeout: Duration,
    pub kafka_producer_acks: String,
    pub kafka_producer_idempotence: bool,
    pub kafka_producer_message_max_bytes: usize,
    pub kafka_topic_management_enabled: bool,
    pub kafka_topic_partitions: i32,
    pub kafka_topic_replication_factor: i32,
    pub producer_queue_messages: usize,
    pub consumer_queue_kbytes: usize,
    pub consumer_partition_fetch_max_bytes: usize,
    pub consumer_fetch_max_bytes: usize,
    pub consumer_record_max_bytes: usize,
    pub redis_url: String,
    pub redis_leader_key: String,
    pub redis_schedule_prefix: String,
    pub redis_idempotency_prefix: String,
    pub redis_aggregation_prefix: String,
    pub node_id: String,
    pub leader_lock_ttl_ms: u64,
    pub leader_renew_interval: Duration,
    pub scheduler_tick_interval: Duration,
    pub execution_lease_ttl: Duration,
    pub execution_lease_renew_interval: Duration,
    pub execution_terminal_retention: Duration,
    pub worker_max_retries: u32,
    pub worker_retry_base_delay: Duration,
    pub worker_retry_max_delay: Duration,
    pub retry_queue_capacity: usize,
    pub aggregation_enabled: bool,
    pub aggregation_partial_timeout: Duration,
    pub aggregation_retention: Duration,
    pub aggregation_scan_interval: Duration,
    pub aggregation_scan_batch: usize,
    pub aggregation_max_active_runs: usize,
    pub aggregation_max_error_kinds: usize,
    pub startup_deadline: Duration,
    pub shutdown_drain_timeout: Duration,
    pub pulse_endpoint: String,
    pub scenarios_file: Option<String>,
    pub grpc_descriptor_set: Option<String>,
    pub grpc_connect_timeout: Duration,
    pub grpc_request_timeout: Duration,
    pub grpc_scenario_timeout: Duration,
    pub max_duration: Duration,
    pub max_scenarios_per_sec: f64,
    pub max_concurrency: usize,
    pub dry_run: bool,
    pub startup_burst: usize,
    pub allow_partial_start: bool,
    pub target_allowlist: Vec<String>,
    pub acknowledge_non_local_targets: bool,
    pub metrics_enabled: bool,
    pub metrics_bind: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Environment {
        name: String,
        reason: String,
    },
    File {
        name: String,
        path: String,
        reason: String,
    },
    Invalid {
        name: String,
        reason: String,
    },
}

impl ConfigError {
    fn environment(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Environment {
            name: name.into(),
            reason: reason.into(),
        }
    }

    fn file(name: impl Into<String>, path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::File {
            name: name.into(),
            path: path.into(),
            reason: reason.into(),
        }
    }

    fn invalid(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Invalid {
            name: name.into(),
            reason: reason.into(),
        }
    }
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Environment { name, reason } => {
                write!(f, "failed to read environment setting {name}: {reason}")
            }
            Self::File { name, path, reason } => {
                write!(f, "failed to load {name} from '{path}': {reason}")
            }
            Self::Invalid { name, reason } => {
                write!(f, "invalid configuration for {name}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_sources(
            |name| match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => {
                    Err(ConfigError::environment(name, "value is not valid Unicode"))
                }
            },
            |path| std::fs::read_to_string(path).map_err(|err| err.to_string()),
            format!("node-{}", std::process::id()),
        )
    }

    fn from_sources<E, F>(
        env: E,
        read_file: F,
        default_node_id: String,
    ) -> Result<Self, ConfigError>
    where
        E: FnMut(&str) -> Result<Option<String>, ConfigError>,
        F: FnMut(&str) -> Result<String, String>,
    {
        let mut source = ConfigSource::new(env, read_file);

        let kafka_producer_acks = match source
            .string("PULSE_KAFKA_PRODUCER_ACKS", "all")?
            .to_ascii_lowercase()
            .as_str()
        {
            "all" | "-1" => "all".to_string(),
            _ => {
                return Err(ConfigError::invalid(
                    "PULSE_KAFKA_PRODUCER_ACKS",
                    "must be 'all' or '-1' so terminal publications are durably acknowledged",
                ));
            }
        };

        let target_allowlist =
            parse_allowlist(&source.string("PULSE_TARGET_ALLOWLIST", "localhost,127.0.0.1,::1")?)?;

        let config = Self {
            kafka_brokers: source.string("PULSE_KAFKA_BROKERS", "localhost:9092")?,
            kafka_jobs_topic: source.string("PULSE_KAFKA_JOBS_TOPIC", "pulse.scenario.jobs")?,
            kafka_results_topic: source
                .string("PULSE_KAFKA_RESULTS_TOPIC", "pulse.scenario.results")?,
            kafka_summaries_topic: source
                .string("PULSE_KAFKA_SUMMARIES_TOPIC", "pulse.scenario.summaries")?,
            kafka_dlq_topic: source.string("PULSE_KAFKA_DLQ_TOPIC", "pulse.scenario.dlq")?,
            kafka_group_id: source.string("PULSE_KAFKA_GROUP_ID", "pulse-workers")?,
            kafka_aggregator_group_id: source
                .string("PULSE_KAFKA_AGGREGATOR_GROUP_ID", "pulse-aggregators")?,
            kafka_max_poll_interval: source
                .duration_ms("PULSE_KAFKA_MAX_POLL_INTERVAL_MS", 300_000)?,
            kafka_session_timeout: source.duration_ms("PULSE_KAFKA_SESSION_TIMEOUT_MS", 10_000)?,
            kafka_message_timeout: source.duration_ms("PULSE_KAFKA_MESSAGE_TIMEOUT_MS", 10_000)?,
            kafka_delivery_timeout: source
                .duration_ms("PULSE_KAFKA_DELIVERY_TIMEOUT_MS", 10_000)?,
            kafka_request_timeout: source.duration_ms("PULSE_KAFKA_REQUEST_TIMEOUT_MS", 5_000)?,
            kafka_producer_acks,
            kafka_producer_idempotence: source.boolean("PULSE_KAFKA_PRODUCER_IDEMPOTENCE", true)?,
            kafka_producer_message_max_bytes: source
                .parse("PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES", 1_000_000_usize)?,
            kafka_topic_management_enabled: source
                .boolean("PULSE_KAFKA_TOPIC_MANAGEMENT_ENABLED", false)?,
            kafka_topic_partitions: source.parse("PULSE_KAFKA_TOPIC_PARTITIONS", 3_i32)?,
            kafka_topic_replication_factor: source
                .parse("PULSE_KAFKA_TOPIC_REPLICATION_FACTOR", 1_i32)?,
            producer_queue_messages: source
                .parse("PULSE_KAFKA_PRODUCER_QUEUE_MESSAGES", 1_024_usize)?,
            consumer_queue_kbytes: source
                .parse("PULSE_KAFKA_CONSUMER_QUEUE_KBYTES", 4_096_usize)?,
            consumer_partition_fetch_max_bytes: source.parse(
                "PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES",
                524_288_usize,
            )?,
            consumer_fetch_max_bytes: source
                .parse("PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES", 4_194_304_usize)?,
            consumer_record_max_bytes: source
                .parse("PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES", 1_000_000_usize)?,
            redis_url: source.string_with_file("PULSE_REDIS_URL", "redis://127.0.0.1:6379")?,
            redis_leader_key: source
                .string("PULSE_REDIS_LEADER_KEY", "pulse:{coordination}:leader")?,
            redis_schedule_prefix: source.string(
                "PULSE_REDIS_SCHEDULE_PREFIX",
                "pulse:{coordination}:schedule",
            )?,
            redis_idempotency_prefix: source
                .string("PULSE_REDIS_IDEMPOTENCY_PREFIX", "pulse:dedupe")?,
            redis_aggregation_prefix: source
                .string("PULSE_REDIS_AGGREGATION_PREFIX", "pulse:aggregation")?,
            node_id: source.string("PULSE_NODE_ID", &default_node_id)?,
            leader_lock_ttl_ms: source.parse("PULSE_LEADER_LOCK_TTL_MS", 10_000_u64)?,
            leader_renew_interval: source.duration_ms("PULSE_LEADER_RENEW_INTERVAL_MS", 3_000)?,
            scheduler_tick_interval: source.duration_ms("PULSE_SCHEDULER_TICK_INTERVAL_MS", 500)?,
            execution_lease_ttl: source.duration_ms("PULSE_EXECUTION_LEASE_TTL_MS", 30_000)?,
            execution_lease_renew_interval: source
                .duration_ms("PULSE_EXECUTION_LEASE_RENEW_INTERVAL_MS", 10_000)?,
            execution_terminal_retention: source
                .duration_ms("PULSE_EXECUTION_TERMINAL_RETENTION_MS", 86_400_000)?,
            worker_max_retries: source.parse("PULSE_WORKER_MAX_RETRIES", 2_u32)?,
            worker_retry_base_delay: source.duration_ms("PULSE_WORKER_RETRY_BASE_DELAY_MS", 500)?,
            worker_retry_max_delay: source
                .duration_ms("PULSE_WORKER_RETRY_MAX_DELAY_MS", 30_000)?,
            retry_queue_capacity: source.parse("PULSE_RETRY_QUEUE_CAPACITY", 1_024_usize)?,
            aggregation_enabled: source.boolean("PULSE_AGGREGATION_ENABLED", true)?,
            aggregation_partial_timeout: source
                .duration_ms("PULSE_AGGREGATION_PARTIAL_TIMEOUT_MS", 60_000)?,
            aggregation_retention: source
                .duration_ms("PULSE_AGGREGATION_RETENTION_MS", 86_400_000)?,
            aggregation_scan_interval: source
                .duration_ms("PULSE_AGGREGATION_SCAN_INTERVAL_MS", 1_000)?,
            aggregation_scan_batch: source.parse("PULSE_AGGREGATION_SCAN_BATCH", 128_usize)?,
            aggregation_max_active_runs: source
                .parse("PULSE_AGGREGATION_MAX_ACTIVE_RUNS", 10_000_usize)?,
            aggregation_max_error_kinds: source
                .parse("PULSE_AGGREGATION_MAX_ERROR_KINDS", 64_usize)?,
            startup_deadline: source.duration_ms("PULSE_STARTUP_DEADLINE_MS", 60_000)?,
            shutdown_drain_timeout: source
                .duration_ms("PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS", 30_000)?,
            pulse_endpoint: source.string("PULSE_ENDPOINT", "http://127.0.0.1:8080")?,
            scenarios_file: source.optional_string("PULSE_SCENARIOS_FILE")?,
            grpc_descriptor_set: source.optional_string("PULSE_GRPC_DESCRIPTOR_SET")?,
            grpc_connect_timeout: source.duration_ms("PULSE_GRPC_CONNECT_TIMEOUT_MS", 5_000)?,
            grpc_request_timeout: source.duration_ms("PULSE_GRPC_REQUEST_TIMEOUT_MS", 5_000)?,
            grpc_scenario_timeout: source.duration_ms("PULSE_GRPC_SCENARIO_TIMEOUT_MS", 30_000)?,
            max_duration: source.duration_ms("PULSE_MAX_DURATION_MS", 60_000)?,
            max_scenarios_per_sec: source.parse("PULSE_MAX_SCENARIOS_PER_SEC", 1_000.0_f64)?,
            max_concurrency: source.parse("PULSE_MAX_CONCURRENCY", 256_usize)?,
            dry_run: source.boolean("PULSE_DRY_RUN", false)?,
            startup_burst: source.parse("PULSE_STARTUP_BURST", 0_usize)?,
            allow_partial_start: source.boolean("PULSE_ALLOW_PARTIAL_START", false)?,
            target_allowlist,
            acknowledge_non_local_targets: source
                .boolean("PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS", false)?,
            metrics_enabled: source.boolean("PULSE_METRICS_ENABLED", true)?,
            metrics_bind: source.string("PULSE_METRICS_BIND", "0.0.0.0:9090")?,
        };

        config.validate()?;
        Ok(config)
    }

    pub fn redis_operation_timeout(&self) -> Duration {
        REDIS_OPERATION_TIMEOUT_CEILING.min(self.kafka_request_timeout)
    }

    /// Maximum time one accepted source record or maintenance cycle may keep
    /// the application from polling Kafka. Validation reserves a fixed margin
    /// for executor scheduling and the next poll call.
    pub fn kafka_safe_processing_interval(&self) -> Duration {
        self.kafka_max_poll_interval
            .saturating_sub(KAFKA_POLL_SAFETY_MARGIN)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_brokers(&self.kafka_brokers)?;
        validate_topic("PULSE_KAFKA_JOBS_TOPIC", &self.kafka_jobs_topic)?;
        validate_topic("PULSE_KAFKA_RESULTS_TOPIC", &self.kafka_results_topic)?;
        validate_topic("PULSE_KAFKA_SUMMARIES_TOPIC", &self.kafka_summaries_topic)?;
        validate_topic("PULSE_KAFKA_DLQ_TOPIC", &self.kafka_dlq_topic)?;

        let unique_topics: HashSet<&str> = [
            self.kafka_jobs_topic.as_str(),
            self.kafka_results_topic.as_str(),
            self.kafka_summaries_topic.as_str(),
            self.kafka_dlq_topic.as_str(),
        ]
        .into_iter()
        .collect();
        if unique_topics.len() != 4 {
            return Err(ConfigError::invalid(
                "Kafka topics",
                "jobs, results, summaries, and DLQ topics must be distinct",
            ));
        }

        validate_non_empty("PULSE_KAFKA_GROUP_ID", &self.kafka_group_id)?;
        validate_non_empty(
            "PULSE_KAFKA_AGGREGATOR_GROUP_ID",
            &self.kafka_aggregator_group_id,
        )?;
        if self.kafka_group_id == self.kafka_aggregator_group_id {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_AGGREGATOR_GROUP_ID",
                "must differ from the job-worker consumer group",
            ));
        }
        validate_non_empty("PULSE_REDIS_LEADER_KEY", &self.redis_leader_key)?;
        validate_non_empty("PULSE_REDIS_SCHEDULE_PREFIX", &self.redis_schedule_prefix)?;
        let leader_slot = redis_hash_tag(&self.redis_leader_key).ok_or_else(|| {
            ConfigError::invalid(
                "PULSE_REDIS_LEADER_KEY",
                "must contain a non-empty Redis Cluster hash tag such as '{coordination}'",
            )
        })?;
        let schedule_slot = redis_hash_tag(&self.redis_schedule_prefix).ok_or_else(|| {
            ConfigError::invalid(
                "PULSE_REDIS_SCHEDULE_PREFIX",
                "must contain the same Redis Cluster hash tag as PULSE_REDIS_LEADER_KEY",
            )
        })?;
        if leader_slot != schedule_slot {
            return Err(ConfigError::invalid(
                "Redis coordination hash tag",
                "leader and schedule keys must share one Redis Cluster hash tag",
            ));
        }
        validate_non_empty(
            "PULSE_REDIS_IDEMPOTENCY_PREFIX",
            &self.redis_idempotency_prefix,
        )?;
        validate_non_empty(
            "PULSE_REDIS_AGGREGATION_PREFIX",
            &self.redis_aggregation_prefix,
        )?;
        let unique_redis_prefixes: HashSet<&str> = [
            self.redis_schedule_prefix.as_str(),
            self.redis_idempotency_prefix.as_str(),
            self.redis_aggregation_prefix.as_str(),
        ]
        .into_iter()
        .collect();
        if unique_redis_prefixes.len() != 3 {
            return Err(ConfigError::invalid(
                "Redis prefixes",
                "schedule, execution, and aggregation prefixes must be distinct",
            ));
        }
        validate_non_empty("PULSE_NODE_ID", &self.node_id)?;

        redis::Client::open(self.redis_url.as_str()).map_err(|err| {
            ConfigError::invalid("PULSE_REDIS_URL", format!("invalid Redis URL: {err}"))
        })?;

        validate_duration_upper_bound(
            "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
            self.kafka_max_poll_interval,
            MAX_KAFKA_POLL_INTERVAL,
        )?;
        validate_duration_upper_bound(
            "PULSE_KAFKA_SESSION_TIMEOUT_MS",
            self.kafka_session_timeout,
            MAX_KAFKA_SESSION_TIMEOUT,
        )?;
        if self.kafka_session_timeout >= self.kafka_max_poll_interval {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_SESSION_TIMEOUT_MS",
                "must be less than PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
            ));
        }
        if self.kafka_message_timeout != self.kafka_delivery_timeout {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_MESSAGE_TIMEOUT_MS",
                "must equal PULSE_KAFKA_DELIVERY_TIMEOUT_MS because librdkafka treats them as aliases",
            ));
        }
        if self.kafka_request_timeout > self.kafka_delivery_timeout {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_REQUEST_TIMEOUT_MS",
                "must not exceed the producer delivery timeout",
            ));
        }
        if !matches!(self.kafka_producer_acks.as_str(), "all" | "-1") {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_PRODUCER_ACKS",
                "must be 'all' or '-1' so terminal publications are durably acknowledged",
            ));
        }

        validate_inclusive_i32(
            "PULSE_KAFKA_TOPIC_PARTITIONS",
            self.kafka_topic_partitions,
            1,
            MAX_TOPIC_PARTITIONS,
        )?;
        validate_inclusive_i32(
            "PULSE_KAFKA_TOPIC_REPLICATION_FACTOR",
            self.kafka_topic_replication_factor,
            1,
            MAX_TOPIC_REPLICATION_FACTOR,
        )?;
        validate_capacity(
            "PULSE_KAFKA_PRODUCER_QUEUE_MESSAGES",
            self.producer_queue_messages,
            MAX_QUEUE_CAPACITY,
        )?;
        validate_capacity(
            "PULSE_KAFKA_CONSUMER_QUEUE_KBYTES",
            self.consumer_queue_kbytes,
            MAX_CONSUMER_QUEUE_KBYTES,
        )?;
        validate_capacity(
            "PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES",
            self.kafka_producer_message_max_bytes,
            MAX_KAFKA_MESSAGE_BYTES,
        )?;
        validate_capacity(
            "PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES",
            self.consumer_partition_fetch_max_bytes,
            MAX_KAFKA_MESSAGE_BYTES,
        )?;
        validate_capacity(
            "PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES",
            self.consumer_fetch_max_bytes,
            MAX_KAFKA_MESSAGE_BYTES,
        )?;
        validate_capacity(
            "PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES",
            self.consumer_record_max_bytes,
            MAX_KAFKA_MESSAGE_BYTES,
        )?;
        if self.consumer_fetch_max_bytes < self.consumer_partition_fetch_max_bytes {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES",
                "must be greater than or equal to PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES",
            ));
        }
        if self.consumer_fetch_max_bytes < self.consumer_record_max_bytes {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES",
                "must be less than or equal to PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES",
            ));
        }
        if self.consumer_record_max_bytes < self.kafka_producer_message_max_bytes {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES",
                "must be greater than or equal to PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES so Pulse can consume every record it can publish",
            ));
        }
        let poison_evidence_bytes =
            MAX_POISON_EVIDENCE_FIELD_BYTES
                .checked_mul(2)
                .ok_or_else(|| {
                    ConfigError::invalid(
                        "PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES",
                        "cannot calculate the bounded poison evidence size",
                    )
                })?;
        let poison_record_bound =
            poison_record_size_bound(poison_evidence_bytes).ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES",
                    "cannot calculate a safe base64 poison-record size bound",
                )
            })?;
        if self.kafka_producer_message_max_bytes < poison_record_bound {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES",
                format!(
                    "must be at least {poison_record_bound} bytes to carry the bounded base64 poison evidence envelope"
                ),
            ));
        }
        validate_capacity(
            "PULSE_RETRY_QUEUE_CAPACITY",
            self.retry_queue_capacity,
            MAX_QUEUE_CAPACITY,
        )?;
        validate_capacity(
            "PULSE_AGGREGATION_SCAN_BATCH",
            self.aggregation_scan_batch,
            MAX_QUEUE_CAPACITY,
        )?;
        validate_capacity(
            "PULSE_AGGREGATION_MAX_ACTIVE_RUNS",
            self.aggregation_max_active_runs,
            MAX_QUEUE_CAPACITY,
        )?;
        validate_capacity(
            "PULSE_AGGREGATION_MAX_ERROR_KINDS",
            self.aggregation_max_error_kinds,
            10_000,
        )?;
        if self.aggregation_scan_batch > self.aggregation_max_active_runs {
            return Err(ConfigError::invalid(
                "PULSE_AGGREGATION_SCAN_BATCH",
                "must not exceed PULSE_AGGREGATION_MAX_ACTIVE_RUNS",
            ));
        }
        if self.aggregation_retention <= self.aggregation_partial_timeout {
            return Err(ConfigError::invalid(
                "PULSE_AGGREGATION_RETENTION_MS",
                "must exceed PULSE_AGGREGATION_PARTIAL_TIMEOUT_MS so finalized summaries remain recoverable",
            ));
        }
        let aggregation_discovery_budget = checked_duration_sum(
            "PULSE_AGGREGATION_RETENTION_MS",
            &[self.max_duration, self.aggregation_partial_timeout],
        )?;
        if self.aggregation_retention <= aggregation_discovery_budget {
            return Err(ConfigError::invalid(
                "PULSE_AGGREGATION_RETENTION_MS",
                "must exceed PULSE_MAX_DURATION_MS plus PULSE_AGGREGATION_PARTIAL_TIMEOUT_MS so a zero-result run remains discoverable",
            ));
        }
        if self.aggregation_scan_interval > self.aggregation_partial_timeout {
            return Err(ConfigError::invalid(
                "PULSE_AGGREGATION_SCAN_INTERVAL_MS",
                "must not exceed PULSE_AGGREGATION_PARTIAL_TIMEOUT_MS",
            ));
        }

        if self.worker_max_retries > MAX_WORKER_RETRIES {
            return Err(ConfigError::invalid(
                "PULSE_WORKER_MAX_RETRIES",
                format!("must not exceed {MAX_WORKER_RETRIES}"),
            ));
        }
        if self.worker_retry_base_delay.is_zero() {
            return Err(ConfigError::invalid(
                "PULSE_WORKER_RETRY_BASE_DELAY_MS",
                "must be greater than zero to prevent a tight retry loop",
            ));
        }
        if self.worker_retry_max_delay.is_zero() {
            return Err(ConfigError::invalid(
                "PULSE_WORKER_RETRY_MAX_DELAY_MS",
                "must be greater than zero to prevent a tight retry loop",
            ));
        }
        if self.worker_retry_base_delay > self.worker_retry_max_delay {
            return Err(ConfigError::invalid(
                "PULSE_WORKER_RETRY_BASE_DELAY_MS",
                "must not exceed PULSE_WORKER_RETRY_MAX_DELAY_MS",
            ));
        }
        if self.worker_retry_max_delay >= self.kafka_max_poll_interval {
            return Err(ConfigError::invalid(
                "PULSE_WORKER_RETRY_MAX_DELAY_MS",
                "must be less than PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
            ));
        }

        let leader_renew_budget = self.leader_renew_interval.checked_mul(3).ok_or_else(|| {
            ConfigError::invalid(
                "PULSE_LEADER_RENEW_INTERVAL_MS",
                "three renewal intervals overflow Duration",
            )
        })?;
        let leader_response_budget = self
            .redis_operation_timeout()
            .checked_add(self.leader_renew_interval)
            .and_then(|budget| budget.checked_add(Duration::from_millis(1)))
            .ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_LEADER_RENEW_INTERVAL_MS",
                    "Redis response safety budget overflowed Duration",
                )
            })?;
        let required_leader_ttl = leader_renew_budget.max(leader_response_budget);
        if Duration::from_millis(self.leader_lock_ttl_ms) < required_leader_ttl {
            return Err(ConfigError::invalid(
                "PULSE_LEADER_LOCK_TTL_MS",
                format!(
                    "must cover three renewal intervals and the Redis operation response budget (at least {} ms)",
                    required_leader_ttl.as_millis()
                ),
            ));
        }
        if self.scheduler_tick_interval >= Duration::from_millis(self.leader_lock_ttl_ms) {
            return Err(ConfigError::invalid(
                "PULSE_SCHEDULER_TICK_INTERVAL_MS",
                "must be less than the leader lock TTL",
            ));
        }

        let execution_renew_budget = self
            .execution_lease_renew_interval
            .checked_mul(3)
            .ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_EXECUTION_LEASE_RENEW_INTERVAL_MS",
                    "three renewal intervals overflow Duration",
                )
            })?;
        let execution_response_budget = self
            .redis_operation_timeout()
            .checked_add(self.execution_lease_renew_interval)
            .and_then(|budget| budget.checked_add(Duration::from_millis(1)))
            .ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_EXECUTION_LEASE_RENEW_INTERVAL_MS",
                    "Redis response safety budget overflowed Duration",
                )
            })?;
        let required_execution_ttl = execution_renew_budget.max(execution_response_budget);
        if self.execution_lease_ttl < required_execution_ttl {
            return Err(ConfigError::invalid(
                "PULSE_EXECUTION_LEASE_TTL_MS",
                format!(
                    "must cover three renewal intervals and the Redis operation response budget (at least {} ms)",
                    required_execution_ttl.as_millis()
                ),
            ));
        }
        if self.execution_terminal_retention < self.kafka_max_poll_interval {
            return Err(ConfigError::invalid(
                "PULSE_EXECUTION_TERMINAL_RETENTION_MS",
                "must be at least the Kafka max poll interval so redelivery can verify terminal state",
            ));
        }

        validate_duration_upper_bound(
            "PULSE_MAX_DURATION_MS",
            self.max_duration,
            MAX_CONFIGURED_DURATION,
        )?;
        if !self.max_scenarios_per_sec.is_finite() || self.max_scenarios_per_sec <= 0.0 {
            return Err(ConfigError::invalid(
                "PULSE_MAX_SCENARIOS_PER_SEC",
                "must be finite and greater than zero",
            ));
        }
        if self.max_scenarios_per_sec > MAX_SAFETY_RATE {
            return Err(ConfigError::invalid(
                "PULSE_MAX_SCENARIOS_PER_SEC",
                format!("must not exceed {MAX_SAFETY_RATE}"),
            ));
        }
        if self.max_concurrency == 0 || self.max_concurrency > MAX_SAFETY_CONCURRENCY {
            return Err(ConfigError::invalid(
                "PULSE_MAX_CONCURRENCY",
                format!("must be between 1 and {MAX_SAFETY_CONCURRENCY}"),
            ));
        }
        if self.startup_burst > self.max_concurrency {
            return Err(ConfigError::invalid(
                "PULSE_STARTUP_BURST",
                "must not exceed PULSE_MAX_CONCURRENCY",
            ));
        }

        if self.grpc_request_timeout > self.grpc_scenario_timeout {
            return Err(ConfigError::invalid(
                "PULSE_GRPC_REQUEST_TIMEOUT_MS",
                "must not exceed PULSE_GRPC_SCENARIO_TIMEOUT_MS",
            ));
        }
        let startup_dependency_budget = checked_duration_sum(
            "PULSE_STARTUP_DEADLINE_MS",
            &[self.grpc_connect_timeout, self.kafka_request_timeout],
        )?;
        if self.startup_deadline < startup_dependency_budget {
            return Err(ConfigError::invalid(
                "PULSE_STARTUP_DEADLINE_MS",
                "must cover the gRPC connect and Kafka request timeout budgets",
            ));
        }
        let shutdown_settlement_budget = checked_duration_sum(
            "PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS",
            &[self.kafka_delivery_timeout, self.kafka_request_timeout],
        )?;
        if self.shutdown_drain_timeout < shutdown_settlement_budget {
            return Err(ConfigError::invalid(
                "PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS",
                "must cover Kafka delivery and request timeout budgets",
            ));
        }

        let local_attempts = self.worker_max_retries.saturating_add(1);
        let result_then_retry_delivery_budget = self
            .kafka_delivery_timeout
            .checked_mul(local_attempts.saturating_mul(2))
            .ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
                    "bounded result/retry delivery budget overflowed Duration",
                )
            })?;
        let completion_request_budget = self
            .kafka_request_timeout
            .checked_mul(local_attempts)
            .ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
                    "bounded completion request budget overflowed Duration",
                )
            })?;
        let local_backoff_budget = retry_backoff_upper_bound(
            self.worker_retry_base_delay,
            self.worker_retry_max_delay,
            self.worker_max_retries,
        )?
        .checked_mul(3)
        .ok_or_else(|| {
            ConfigError::invalid(
                "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
                "bounded publication/completion backoff budget overflowed Duration",
            )
        })?;
        let bounded_job_and_settlement = checked_duration_sum(
            "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
            &[
                self.max_duration,
                self.grpc_scenario_timeout,
                self.worker_retry_max_delay,
                result_then_retry_delivery_budget,
                completion_request_budget,
                local_backoff_budget,
            ],
        )?;
        let required_poll_interval = bounded_job_and_settlement
            .checked_add(KAFKA_POLL_SAFETY_MARGIN)
            .ok_or_else(|| {
                ConfigError::invalid(
                    "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
                    "bounded job, settlement, and poll safety margin overflowed Duration",
                )
            })?;
        if self.kafka_max_poll_interval <= required_poll_interval {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
                format!(
                    "must exceed the bounded job/settlement budget plus the {} ms poll safety margin (more than {} ms total)",
                    KAFKA_POLL_SAFETY_MARGIN.as_millis(),
                    required_poll_interval.as_millis()
                ),
            ));
        }

        if self.target_allowlist.is_empty() {
            return Err(ConfigError::invalid(
                "PULSE_TARGET_ALLOWLIST",
                "must contain at least one host",
            ));
        }
        for host in &self.target_allowlist {
            validate_allowlist_host(host)?;
        }

        self.metrics_bind.parse::<SocketAddr>().map_err(|err| {
            ConfigError::invalid(
                "PULSE_METRICS_BIND",
                format!("must be a numeric socket address: {err}"),
            )
        })?;
        self.validate_target_endpoint(&self.pulse_endpoint)?;

        Ok(())
    }

    pub fn validate_target_endpoint(&self, endpoint: &str) -> Result<(), ConfigError> {
        let host = endpoint_host("target endpoint", endpoint)?;
        if self.acknowledge_non_local_targets
            || self
                .target_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&host))
        {
            return Ok(());
        }

        Err(ConfigError::invalid(
            "target endpoint",
            format!(
                "host '{host}' is not allowlisted; add it to PULSE_TARGET_ALLOWLIST or explicitly set PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS=true"
            ),
        ))
    }
}

struct ConfigSource<E, F>
where
    E: FnMut(&str) -> Result<Option<String>, ConfigError>,
    F: FnMut(&str) -> Result<String, String>,
{
    env: E,
    read_file: F,
}

impl<E, F> ConfigSource<E, F>
where
    E: FnMut(&str) -> Result<Option<String>, ConfigError>,
    F: FnMut(&str) -> Result<String, String>,
{
    fn new(env: E, read_file: F) -> Self {
        Self { env, read_file }
    }

    fn raw(&mut self, name: &str) -> Result<Option<String>, ConfigError> {
        (self.env)(name)
    }

    fn string(&mut self, name: &str, default: &str) -> Result<String, ConfigError> {
        match self.raw(name)? {
            Some(value) => trimmed_non_empty(name, &value),
            None => trimmed_non_empty(name, default),
        }
    }

    fn optional_string(&mut self, name: &str) -> Result<Option<String>, ConfigError> {
        self.raw(name)?
            .map(|value| trimmed_non_empty(name, &value))
            .transpose()
    }

    fn string_with_file(&mut self, name: &str, default: &str) -> Result<String, ConfigError> {
        let file_name = format!("{name}_FILE");
        let direct = self.raw(name)?;
        let file_path = self.raw(&file_name)?;

        if direct.is_some() && file_path.is_some() {
            return Err(ConfigError::invalid(
                name,
                format!("cannot be set together with {file_name}"),
            ));
        }

        if let Some(value) = direct {
            return trimmed_non_empty(name, &value);
        }

        if let Some(path) = file_path {
            let path = trimmed_non_empty(&file_name, &path)?;
            let value =
                (self.read_file)(&path).map_err(|reason| ConfigError::file(name, &path, reason))?;
            return trimmed_non_empty(name, &value)
                .map_err(|_| ConfigError::file(name, path, "file is empty or whitespace-only"));
        }

        trimmed_non_empty(name, default)
    }

    fn parse<T>(&mut self, name: &str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr,
    {
        let Some(raw) = self.raw(name)? else {
            return Ok(default);
        };
        let value = trimmed_non_empty(name, &raw)?;
        value.parse::<T>().map_err(|_| {
            ConfigError::invalid(
                name,
                format!("must be a valid {}", std::any::type_name::<T>()),
            )
        })
    }

    fn duration_ms(&mut self, name: &str, default_ms: u64) -> Result<Duration, ConfigError> {
        let millis: u64 = self.parse(name, default_ms)?;
        if millis == 0 {
            return Err(ConfigError::invalid(name, "must be greater than zero"));
        }
        Ok(Duration::from_millis(millis))
    }

    fn boolean(&mut self, name: &str, default: bool) -> Result<bool, ConfigError> {
        let Some(raw) = self.raw(name)? else {
            return Ok(default);
        };
        let value = trimmed_non_empty(name, &raw)?.to_ascii_lowercase();
        match value.as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(ConfigError::invalid(
                name,
                "must be one of: true, false, 1, 0",
            )),
        }
    }
}

fn trimmed_non_empty(name: &str, value: &str) -> Result<String, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::invalid(
            name,
            "must not be empty or whitespace-only",
        ));
    }
    Ok(trimmed.to_string())
}

fn validate_non_empty(name: &str, value: &str) -> Result<(), ConfigError> {
    trimmed_non_empty(name, value).map(|_| ())
}

fn redis_hash_tag(value: &str) -> Option<&str> {
    let open = value.find('{')?;
    let remainder = &value[open + 1..];
    let close = remainder.find('}')?;
    (close > 0).then_some(&remainder[..close])
}

fn validate_brokers(brokers: &str) -> Result<(), ConfigError> {
    validate_non_empty("PULSE_KAFKA_BROKERS", brokers)?;
    for broker in brokers.split(',') {
        let broker = broker.trim();
        if broker.is_empty() || broker.contains("://") {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_BROKERS",
                "must be a comma-separated list of host:port authorities without URL schemes",
            ));
        }
        let uri = Endpoint::from_shared(format!("http://{broker}")).map_err(|_| {
            ConfigError::invalid(
                "PULSE_KAFKA_BROKERS",
                "contains an invalid broker authority",
            )
        })?;
        if uri.uri().host().is_none()
            || uri.uri().port_u16().is_none()
            || !matches!(uri.uri().path(), "" | "/")
            || uri.uri().query().is_some()
        {
            return Err(ConfigError::invalid(
                "PULSE_KAFKA_BROKERS",
                "every broker must include a host and numeric port",
            ));
        }
    }
    Ok(())
}

fn validate_topic(name: &str, topic: &str) -> Result<(), ConfigError> {
    validate_non_empty(name, topic)?;
    if topic.len() > 249 {
        return Err(ConfigError::invalid(name, "must be at most 249 bytes"));
    }
    if !topic
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ConfigError::invalid(
            name,
            "may contain only ASCII letters, digits, '.', '_', and '-'",
        ));
    }
    Ok(())
}

fn validate_capacity(name: &str, value: usize, maximum: usize) -> Result<(), ConfigError> {
    if value == 0 || value > maximum {
        return Err(ConfigError::invalid(
            name,
            format!("must be between 1 and {maximum}"),
        ));
    }
    Ok(())
}

fn poison_record_size_bound(source_message_bytes: usize) -> Option<usize> {
    // Encoding the two bounded evidence prefixes as optional base64 fields can
    // add at most one extra padded quartet beyond encoding their combined bytes.
    // The fixed allowance covers JSON field names, original-size/truncation
    // metadata, bounded reason/source identifiers, the DLQ key, and framing.
    source_message_bytes
        .checked_add(2)?
        .checked_div(3)?
        .checked_mul(4)?
        .checked_add(4)?
        .checked_add(POISON_ENVELOPE_OVERHEAD_BYTES)
}

fn validate_inclusive_i32(
    name: &str,
    value: i32,
    minimum: i32,
    maximum: i32,
) -> Result<(), ConfigError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(ConfigError::invalid(
            name,
            format!("must be between {minimum} and {maximum}"),
        ));
    }
    Ok(())
}

fn validate_duration_upper_bound(
    name: &str,
    value: Duration,
    maximum: Duration,
) -> Result<(), ConfigError> {
    if value.is_zero() || value > maximum {
        return Err(ConfigError::invalid(
            name,
            format!(
                "must be greater than zero and no more than {} ms",
                maximum.as_millis()
            ),
        ));
    }
    Ok(())
}

fn checked_duration_sum(name: &str, values: &[Duration]) -> Result<Duration, ConfigError> {
    values.iter().try_fold(Duration::ZERO, |sum, value| {
        sum.checked_add(*value)
            .ok_or_else(|| ConfigError::invalid(name, "duration budget overflowed"))
    })
}

fn retry_backoff_upper_bound(
    base: Duration,
    maximum: Duration,
    retry_count: u32,
) -> Result<Duration, ConfigError> {
    let mut total = Duration::ZERO;
    for attempt in 0..retry_count {
        let multiplier = 1_u32 << attempt.min(16);
        let exponential = base.saturating_mul(multiplier).min(maximum);
        // Runtime jitter is bounded at 120%, followed by the same maximum cap.
        let upper = (exponential.saturating_mul(6) / 5).min(maximum);
        total = total.checked_add(upper).ok_or_else(|| {
            ConfigError::invalid(
                "PULSE_KAFKA_MAX_POLL_INTERVAL_MS",
                "bounded retry backoff sum overflowed Duration",
            )
        })?;
    }
    Ok(total)
}

fn parse_allowlist(raw: &str) -> Result<Vec<String>, ConfigError> {
    let mut hosts = Vec::new();
    let mut seen = HashSet::new();
    for raw_host in raw.split(',') {
        let host = raw_host.trim().to_ascii_lowercase();
        if host.is_empty() {
            return Err(ConfigError::invalid(
                "PULSE_TARGET_ALLOWLIST",
                "must not contain empty entries",
            ));
        }
        validate_allowlist_host(&host)?;
        if seen.insert(host.clone()) {
            hosts.push(host);
        }
    }
    if hosts.is_empty() {
        return Err(ConfigError::invalid(
            "PULSE_TARGET_ALLOWLIST",
            "must contain at least one host",
        ));
    }
    Ok(hosts)
}

fn validate_allowlist_host(host: &str) -> Result<(), ConfigError> {
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    if host.len() > 253
        || host.starts_with('.')
        || host.ends_with('.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(ConfigError::invalid(
            "PULSE_TARGET_ALLOWLIST",
            "entries must be hostnames or IP addresses without schemes, paths, or ports",
        ));
    }
    Ok(())
}

fn endpoint_host(name: &str, value: &str) -> Result<String, ConfigError> {
    let endpoint = Endpoint::from_shared(value.to_string())
        .map_err(|err| ConfigError::invalid(name, format!("invalid endpoint URL: {err}")))?;
    let uri = endpoint.uri();
    match uri.scheme_str() {
        Some("http") => {}
        Some("https") => {
            return Err(ConfigError::invalid(
                name,
                "https endpoints are not supported because this Pulse build has no gRPC TLS transport; use http:// only on a trusted network",
            ));
        }
        _ => {
            return Err(ConfigError::invalid(
                name,
                "endpoint URL scheme must be http; this Pulse build supports plaintext gRPC only",
            ));
        }
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return Err(ConfigError::invalid(
            name,
            "endpoint URL must not contain a path or query",
        ));
    }
    uri.host()
        .map(|host| {
            host.strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host)
                .to_ascii_lowercase()
        })
        .ok_or_else(|| ConfigError::invalid(name, "endpoint URL must include a host"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{AppConfig, ConfigError};

    fn config_from(entries: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
        config_from_with_files(entries, &[])
    }

    fn config_from_with_files(
        entries: &[(&str, &str)],
        files: &[(&str, Result<&str, &str>)],
    ) -> Result<AppConfig, ConfigError> {
        let environment: HashMap<String, String> = entries
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        let file_contents: HashMap<String, Result<String, String>> = files
            .iter()
            .map(|(path, result)| {
                (
                    (*path).to_string(),
                    result
                        .as_ref()
                        .map(|value| (*value).to_string())
                        .map_err(|error| (*error).to_string()),
                )
            })
            .collect();

        AppConfig::from_sources(
            move |name| Ok(environment.get(name).cloned()),
            move |path| {
                file_contents
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| Err("test file does not exist".to_string()))
            },
            "node-test".to_string(),
        )
    }

    fn invalid_name(error: ConfigError) -> String {
        match error {
            ConfigError::Environment { name, .. }
            | ConfigError::File { name, .. }
            | ConfigError::Invalid { name, .. } => name,
        }
    }

    fn invalid_reason(error: ConfigError) -> String {
        match error {
            ConfigError::Environment { reason, .. }
            | ConfigError::File { reason, .. }
            | ConfigError::Invalid { reason, .. } => reason,
        }
    }

    #[test]
    fn defaults_are_local_bounded_and_non_mutating() {
        let config = config_from(&[]).expect("defaults must validate");

        assert_eq!(config.kafka_brokers, "localhost:9092");
        assert_eq!(config.kafka_max_poll_interval, Duration::from_secs(300));
        assert_eq!(
            config.kafka_safe_processing_interval(),
            Duration::from_secs(299)
        );
        assert_eq!(config.kafka_session_timeout, Duration::from_secs(10));
        assert_eq!(config.kafka_producer_acks, "all");
        assert!(config.kafka_producer_idempotence);
        assert!(!config.kafka_topic_management_enabled);
        assert_eq!(config.kafka_topic_partitions, 3);
        assert_eq!(config.kafka_topic_replication_factor, 1);
        assert_eq!(config.producer_queue_messages, 1_024);
        assert_eq!(config.consumer_queue_kbytes, 4_096);
        assert_eq!(config.kafka_producer_message_max_bytes, 1_000_000);
        assert_eq!(config.consumer_partition_fetch_max_bytes, 524_288);
        assert_eq!(config.consumer_fetch_max_bytes, 4_194_304);
        assert_eq!(config.consumer_record_max_bytes, 1_000_000);
        assert_eq!(config.execution_lease_ttl, Duration::from_secs(30));
        assert_eq!(
            config.execution_terminal_retention,
            Duration::from_secs(86_400)
        );
        assert_eq!(config.worker_retry_max_delay, Duration::from_secs(30));
        assert_eq!(config.retry_queue_capacity, 1_024);
        assert_eq!(config.startup_deadline, Duration::from_secs(60));
        assert_eq!(config.shutdown_drain_timeout, Duration::from_secs(30));
        assert_eq!(config.grpc_connect_timeout, Duration::from_secs(5));
        assert_eq!(config.grpc_request_timeout, Duration::from_secs(5));
        assert_eq!(config.grpc_scenario_timeout, Duration::from_secs(30));
        assert_eq!(config.max_duration, Duration::from_secs(60));
        assert_eq!(config.max_scenarios_per_sec, 1_000.0);
        assert_eq!(config.max_concurrency, 256);
        assert!(!config.dry_run);
        assert_eq!(config.startup_burst, 0);
        assert!(!config.allow_partial_start);
        assert!(!config.acknowledge_non_local_targets);
        assert_eq!(config.target_allowlist, ["localhost", "127.0.0.1", "::1"]);
    }

    #[test]
    fn malformed_present_numeric_value_is_an_error() {
        let error = config_from(&[("PULSE_WORKER_MAX_RETRIES", "two")])
            .expect_err("malformed value must not use a default");
        assert_eq!(invalid_name(error), "PULSE_WORKER_MAX_RETRIES");
    }

    #[test]
    fn malformed_present_boolean_value_is_an_error() {
        let error = config_from(&[("PULSE_DRY_RUN", "sometimes")])
            .expect_err("malformed bool must not use a default");
        assert_eq!(invalid_name(error), "PULSE_DRY_RUN");
    }

    #[test]
    fn empty_present_optional_value_is_an_error() {
        let error = config_from(&[("PULSE_GRPC_DESCRIPTOR_SET", "  ")])
            .expect_err("present optional settings must be meaningful");
        assert_eq!(invalid_name(error), "PULSE_GRPC_DESCRIPTOR_SET");
    }

    #[test]
    fn file_backed_setting_is_trimmed_and_never_silently_ignored() {
        let config = config_from_with_files(
            &[("PULSE_REDIS_URL_FILE", "/run/secrets/redis")],
            &[("/run/secrets/redis", Ok(" redis://127.0.0.1:6379\n"))],
        )
        .expect("valid file-backed value");
        assert_eq!(config.redis_url, "redis://127.0.0.1:6379");

        let unreadable = config_from_with_files(
            &[("PULSE_REDIS_URL_FILE", "/run/secrets/redis")],
            &[("/run/secrets/redis", Err("permission denied"))],
        )
        .expect_err("unreadable secret must fail");
        assert!(matches!(unreadable, ConfigError::File { .. }));

        let empty = config_from_with_files(
            &[("PULSE_REDIS_URL_FILE", "/run/secrets/redis")],
            &[("/run/secrets/redis", Ok(" \n"))],
        )
        .expect_err("empty secret must fail");
        assert!(matches!(empty, ConfigError::File { .. }));
    }

    #[test]
    fn direct_and_file_backed_values_are_rejected_as_ambiguous() {
        let error = config_from(&[
            ("PULSE_REDIS_URL", "redis://127.0.0.1:6379"),
            ("PULSE_REDIS_URL_FILE", "/run/secrets/redis"),
        ])
        .expect_err("ambiguous secret sources must fail");
        assert_eq!(invalid_name(error), "PULSE_REDIS_URL");
    }

    #[test]
    fn ttl_and_renewal_relationships_are_validated() {
        let leader_error = config_from(&[("PULSE_LEADER_RENEW_INTERVAL_MS", "4000")])
            .expect_err("leader TTL must tolerate missed renewals");
        assert_eq!(invalid_name(leader_error), "PULSE_LEADER_LOCK_TTL_MS");

        let lease_error = config_from(&[("PULSE_EXECUTION_LEASE_TTL_MS", "29999")])
            .expect_err("lease TTL must tolerate missed renewals");
        assert_eq!(invalid_name(lease_error), "PULSE_EXECUTION_LEASE_TTL_MS");

        let slow_leader_response = config_from(&[
            ("PULSE_LEADER_LOCK_TTL_MS", "300"),
            ("PULSE_LEADER_RENEW_INTERVAL_MS", "100"),
        ])
        .expect_err("Redis response latency must not consume the entire leader lease");
        assert_eq!(
            invalid_name(slow_leader_response),
            "PULSE_LEADER_LOCK_TTL_MS"
        );

        let slow_execution_response = config_from(&[
            ("PULSE_EXECUTION_LEASE_TTL_MS", "300"),
            ("PULSE_EXECUTION_LEASE_RENEW_INTERVAL_MS", "100"),
        ])
        .expect_err("Redis response latency must not consume the entire execution lease");
        assert_eq!(
            invalid_name(slow_execution_response),
            "PULSE_EXECUTION_LEASE_TTL_MS"
        );
    }

    #[test]
    fn redis_coordination_keys_require_one_shared_cluster_hash_tag() {
        let missing = config_from(&[("PULSE_REDIS_LEADER_KEY", "pulse:leader")])
            .expect_err("leader key without a hash tag can cross Redis Cluster slots");
        assert_eq!(invalid_name(missing), "PULSE_REDIS_LEADER_KEY");

        let empty = config_from(&[("PULSE_REDIS_LEADER_KEY", "pulse:{}:leader")])
            .expect_err("an empty Redis hash tag hashes the complete key");
        assert_eq!(invalid_name(empty), "PULSE_REDIS_LEADER_KEY");

        let mismatched = config_from(&[
            ("PULSE_REDIS_LEADER_KEY", "pulse:{leaders}:leader"),
            ("PULSE_REDIS_SCHEDULE_PREFIX", "pulse:{schedules}:schedule"),
        ])
        .expect_err("multi-key coordination scripts require one Redis Cluster slot");
        assert_eq!(invalid_name(mismatched), "Redis coordination hash tag");

        config_from(&[
            ("PULSE_REDIS_LEADER_KEY", "pulse:{shared}:leader"),
            ("PULSE_REDIS_SCHEDULE_PREFIX", "pulse:{shared}:schedule"),
        ])
        .expect("matching non-empty hash tags are cluster-safe");
    }

    #[test]
    fn retry_and_shutdown_budgets_are_validated() {
        let zero_retry = config_from(&[("PULSE_WORKER_RETRY_BASE_DELAY_MS", "0")])
            .expect_err("zero retry backoff would create a retry storm");
        assert_eq!(invalid_name(zero_retry), "PULSE_WORKER_RETRY_BASE_DELAY_MS");

        let retry_error = config_from(&[("PULSE_WORKER_RETRY_BASE_DELAY_MS", "30001")])
            .expect_err("base retry delay cannot exceed max delay");
        assert_eq!(
            invalid_name(retry_error),
            "PULSE_WORKER_RETRY_BASE_DELAY_MS"
        );

        let shutdown_error = config_from(&[("PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS", "14999")])
            .expect_err("drain must cover settlement");
        assert_eq!(
            invalid_name(shutdown_error),
            "PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS"
        );
    }

    #[test]
    fn max_poll_must_cover_bounded_job_and_settlement() {
        let error = config_from(&[("PULSE_KAFKA_MAX_POLL_INTERVAL_MS", "120000")])
            .expect_err("max poll must be strictly larger than the complete budget");
        assert_eq!(invalid_name(error), "PULSE_KAFKA_MAX_POLL_INTERVAL_MS");
    }

    #[test]
    fn finite_rate_and_capacity_bounds_are_enforced() {
        let rate_error = config_from(&[("PULSE_MAX_SCENARIOS_PER_SEC", "NaN")])
            .expect_err("NaN is not a safe ceiling");
        assert_eq!(invalid_name(rate_error), "PULSE_MAX_SCENARIOS_PER_SEC");

        let queue_error = config_from(&[("PULSE_RETRY_QUEUE_CAPACITY", "0")])
            .expect_err("queues must be bounded and nonzero");
        assert_eq!(invalid_name(queue_error), "PULSE_RETRY_QUEUE_CAPACITY");

        let burst_error = config_from(&[("PULSE_STARTUP_BURST", "257")])
            .expect_err("startup burst must respect concurrency ceiling");
        assert_eq!(invalid_name(burst_error), "PULSE_STARTUP_BURST");
    }

    #[test]
    fn kafka_topics_brokers_and_acks_are_strict() {
        let broker_error = config_from(&[("PULSE_KAFKA_BROKERS", "localhost")])
            .expect_err("broker port is required");
        assert_eq!(invalid_name(broker_error), "PULSE_KAFKA_BROKERS");

        let topic_error = config_from(&[("PULSE_KAFKA_DLQ_TOPIC", "pulse bad topic")])
            .expect_err("invalid topic characters must fail");
        assert_eq!(invalid_name(topic_error), "PULSE_KAFKA_DLQ_TOPIC");

        let duplicate_topic = config_from(&[("PULSE_KAFKA_DLQ_TOPIC", "pulse.scenario.results")])
            .expect_err("terminal topics must not alias");
        assert_eq!(invalid_name(duplicate_topic), "Kafka topics");

        let ack_error = config_from(&[("PULSE_KAFKA_PRODUCER_ACKS", "1")])
            .expect_err("weak acknowledgements must fail");
        assert_eq!(invalid_name(ack_error), "PULSE_KAFKA_PRODUCER_ACKS");

        let normalized = config_from(&[("PULSE_KAFKA_PRODUCER_ACKS", "-1")])
            .expect("-1 is the librdkafka spelling for all");
        assert_eq!(normalized.kafka_producer_acks, "all");
    }

    #[test]
    fn kafka_fetch_and_poison_publication_size_bounds_are_consistent() {
        let total_too_small = config_from(&[
            ("PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES", "1048576"),
            ("PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES", "524288"),
            ("PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES", "1500000"),
        ])
        .expect_err("total fetch bound must cover one partition fetch");
        assert_eq!(
            invalid_name(total_too_small),
            "PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES"
        );

        let poison_too_large = config_from(&[
            ("PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES", "800000"),
            ("PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES", "800000"),
            ("PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES", "700000"),
        ])
        .expect_err("producer must carry base64 poison evidence");
        assert_eq!(
            invalid_name(poison_too_large),
            "PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES"
        );

        config_from(&[
            ("PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES", "800000"),
            ("PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES", "4000000"),
            ("PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES", "800000"),
            ("PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES", "800000"),
        ])
        .expect("independent, compatible byte bounds must validate");

        let record_too_small_for_local_publications = config_from(&[
            ("PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES", "900000"),
            ("PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES", "899999"),
        ])
        .expect_err("consumer record bound must cover locally publishable records");
        assert_eq!(
            invalid_name(record_too_small_for_local_publications),
            "PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES"
        );

        let record_larger_than_fetch = config_from(&[
            ("PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES", "1200000"),
            ("PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES", "1200001"),
        ])
        .expect_err("record bound must fit the total fetch bound");
        assert_eq!(
            invalid_name(record_larger_than_fetch),
            "PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES"
        );
    }

    #[test]
    fn bind_and_endpoint_urls_are_validated_without_network_access() {
        let bind_error = config_from(&[("PULSE_METRICS_BIND", "localhost:9090")])
            .expect_err("bind must be a numeric socket address");
        assert_eq!(invalid_name(bind_error), "PULSE_METRICS_BIND");

        let endpoint_error = config_from(&[("PULSE_ENDPOINT", "127.0.0.1:8080")])
            .expect_err("endpoint requires a URL scheme");
        assert_eq!(invalid_name(endpoint_error), "target endpoint");

        let path_error = config_from(&[("PULSE_ENDPOINT", "http://127.0.0.1:8080/path")])
            .expect_err("target endpoint must be an origin");
        assert_eq!(invalid_name(path_error), "target endpoint");

        let tls_error = config_from(&[("PULSE_ENDPOINT", "https://127.0.0.1:8080")])
            .expect_err("TLS must not be implied when tonic's TLS transport is disabled");
        let reason = invalid_reason(tls_error);
        assert!(reason.contains("no gRPC TLS transport"), "{reason}");
        assert!(reason.contains("use http:// only"), "{reason}");
    }

    #[test]
    fn non_local_targets_require_allowlisting_or_explicit_acknowledgement() {
        let blocked = config_from(&[("PULSE_ENDPOINT", "http://example.com")])
            .expect_err("remote target must not be implicit");
        assert_eq!(invalid_name(blocked), "target endpoint");

        let allowlisted = config_from(&[
            ("PULSE_ENDPOINT", "http://example.com"),
            ("PULSE_TARGET_ALLOWLIST", "localhost,example.com"),
        ])
        .expect("explicit allowlist permits the target");
        allowlisted
            .validate_target_endpoint("http://example.com")
            .expect("allowlisted endpoint remains valid");

        let acknowledged = config_from(&[
            ("PULSE_ENDPOINT", "http://example.com"),
            ("PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS", "true"),
        ])
        .expect("explicit acknowledgement permits remote targets");
        acknowledged
            .validate_target_endpoint("http://another.example")
            .expect("acknowledgement covers scenario endpoints too");
    }
}
