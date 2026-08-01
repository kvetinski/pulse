use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rdkafka::ClientConfig;
use rdkafka::Message;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::{ClientContext, DefaultClientContext};
use rdkafka::consumer::{
    BaseConsumer, CommitMode, Consumer, ConsumerContext, Rebalance, StreamConsumer,
};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use rdkafka::types::RDKafkaErrorCode;

use crate::application::aggregation_service::{CommitableResult, ResultConsumer, SummaryPublisher};
use crate::application::service::{
    CommitableJob, DlqPublisher, JobConsumer, JobPublisher, ResultPublisher,
};
use crate::domain::contracts::{
    CURRENT_CONTRACT_VERSION, FailedScenarioJob, MAX_CONTRACT_ATTEMPT, MAX_CONTRACT_ID_BYTES,
    MAX_CONTRACT_SLICES, MAX_POISON_EVIDENCE_FIELD_BYTES, PoisonMessageRecord, ScenarioJob,
    ScenarioRunResult, ScenarioRunSummaryEvent, build_terminal_event_id, now_unix_ms,
    validate_contract_version,
};
use crate::domain::error::ContractError;
use crate::infrastructure::metrics as runtime_metrics;

const POISON_REASON_TRUNCATION_MARKER: &str = "...[truncated]";
const KAFKA_RESPONSE_OVERHEAD_BYTES: usize = 512;

#[derive(Debug, Default)]
struct RebalanceFence {
    epoch: AtomicU64,
}

impl RebalanceFence {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

impl ClientContext for RebalanceFence {}

impl ConsumerContext for RebalanceFence {
    fn pre_rebalance(&self, rebalance: &Rebalance<'_>) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let outcome = match rebalance {
            Rebalance::Assign(_) => "assign",
            Rebalance::Revoke(_) => "revoke",
            Rebalance::Error(_) => "error",
        };
        tracing::warn!(
            rebalance_epoch = epoch,
            outcome,
            "Kafka rebalance fenced records buffered under the previous assignment epoch"
        );
    }
}

type FencedStreamConsumer = StreamConsumer<RebalanceFence>;

/// Settings that have message-count units and apply to all Pulse Kafka producers.
///
/// `message_timeout` and `delivery_timeout` must be equal because librdkafka treats
/// `message.timeout.ms` and `delivery.timeout.ms` as aliases. They are retained as
/// separate fields so the aliasing is explicit at the configuration boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KafkaProducerConfig {
    pub queue_capacity_messages: usize,
    pub message_max_bytes: usize,
    pub message_timeout: Duration,
    pub delivery_timeout: Duration,
    pub request_timeout: Duration,
    pub acks: String,
    pub enable_idempotence: bool,
}

impl Default for KafkaProducerConfig {
    fn default() -> Self {
        Self {
            queue_capacity_messages: 1_024,
            message_max_bytes: 1_000_000,
            message_timeout: Duration::from_secs(10),
            delivery_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(5),
            acks: "all".to_string(),
            enable_idempotence: true,
        }
    }
}

impl KafkaProducerConfig {
    fn with_queue_capacity(queue_capacity_messages: usize) -> Self {
        Self {
            queue_capacity_messages,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_capacity(
            "Kafka producer queue capacity",
            self.queue_capacity_messages,
        )?;
        validate_capacity("Kafka producer message max bytes", self.message_max_bytes)?;
        validate_duration("Kafka message timeout", self.message_timeout)?;
        validate_duration("Kafka delivery timeout", self.delivery_timeout)?;
        validate_duration("Kafka request timeout", self.request_timeout)?;

        if self.message_timeout != self.delivery_timeout {
            return Err(
                "Kafka message timeout must equal delivery timeout because librdkafka treats message.timeout.ms and delivery.timeout.ms as aliases"
                    .to_string(),
            );
        }
        if self.request_timeout > self.delivery_timeout {
            return Err("Kafka request timeout must not exceed delivery timeout".to_string());
        }
        if !matches!(self.acks.as_str(), "all" | "-1") {
            return Err(
                "Kafka producer acks must be 'all' or '-1' for durable disposition publication"
                    .to_string(),
            );
        }

        Ok(())
    }
}

/// Settings that have byte-prefetch units and apply to the Pulse Kafka consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KafkaConsumerConfig {
    pub max_poll_interval: Duration,
    pub session_timeout: Duration,
    /// Bounds synchronous offset commits and other broker requests.
    pub request_timeout: Duration,
    pub prefetch_kib: usize,
    /// Initial maximum bytes fetched for one topic partition.
    pub partition_fetch_max_bytes: usize,
    /// Maximum bytes returned by one fetch request across partitions.
    pub fetch_max_bytes: usize,
    /// Maximum combined key and payload bytes Pulse will retain and decode for
    /// one consumed record. Kafka fetch limits are not a per-record ceiling:
    /// brokers may return an oversized first record so a partition can make
    /// progress. Pulse therefore enforces this bound before making owned copies.
    pub record_max_bytes: usize,
}

impl Default for KafkaConsumerConfig {
    fn default() -> Self {
        Self {
            max_poll_interval: Duration::from_secs(300),
            session_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(5),
            prefetch_kib: 4_096,
            partition_fetch_max_bytes: 524_288,
            fetch_max_bytes: 4_194_304,
            record_max_bytes: 1_000_000,
        }
    }
}

impl KafkaConsumerConfig {
    fn with_prefetch_kib(prefetch_kib: usize) -> Self {
        Self {
            prefetch_kib,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_duration("Kafka max poll interval", self.max_poll_interval)?;
        validate_duration("Kafka session timeout", self.session_timeout)?;
        validate_duration("Kafka consumer request timeout", self.request_timeout)?;
        validate_capacity("Kafka consumer prefetch KiB", self.prefetch_kib)?;
        validate_capacity(
            "Kafka consumer partition fetch max bytes",
            self.partition_fetch_max_bytes,
        )?;
        validate_capacity("Kafka consumer fetch max bytes", self.fetch_max_bytes)?;
        validate_capacity("Kafka consumer record max bytes", self.record_max_bytes)?;
        let poison_evidence_bytes = MAX_POISON_EVIDENCE_FIELD_BYTES
            .checked_mul(2)
            .ok_or_else(|| "Kafka poison evidence byte bound overflowed".to_string())?;

        if self.session_timeout >= self.max_poll_interval {
            return Err("Kafka session timeout must be less than max poll interval".to_string());
        }
        if self.request_timeout >= self.max_poll_interval {
            return Err(
                "Kafka consumer request timeout must be less than max poll interval".to_string(),
            );
        }
        if self.fetch_max_bytes < self.partition_fetch_max_bytes {
            return Err(
                "Kafka consumer fetch max bytes must be at least the partition fetch max bytes"
                    .to_string(),
            );
        }
        if self.fetch_max_bytes < self.record_max_bytes {
            return Err(
                "Kafka consumer fetch max bytes must be at least the consumed-record max bytes"
                    .to_string(),
            );
        }
        if self.record_max_bytes < poison_evidence_bytes {
            return Err(format!(
                "Kafka consumer record max bytes must be at least {poison_evidence_bytes} to retain bounded key and payload poison prefixes"
            ));
        }
        self.fetch_max_bytes
            .checked_add(KAFKA_RESPONSE_OVERHEAD_BYTES)
            .filter(|bytes| *bytes <= i32::MAX as usize)
            .ok_or_else(|| {
                "Kafka consumer fetch max bytes leaves no room for protocol response overhead"
                    .to_string()
            })?;

        Ok(())
    }
}

fn validate_duration(name: &str, duration: Duration) -> Result<(), String> {
    let milliseconds = duration.as_millis();
    if milliseconds == 0 {
        return Err(format!("{name} must be at least one millisecond"));
    }
    if milliseconds > i32::MAX as u128 {
        return Err(format!(
            "{name} exceeds Kafka's signed 32-bit millisecond limit"
        ));
    }
    Ok(())
}

fn validate_capacity(name: &str, value: usize) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    if value > i32::MAX as usize {
        return Err(format!("{name} exceeds Kafka's signed 32-bit limit"));
    }
    Ok(())
}

fn producer_client_config(
    brokers: &str,
    config: &KafkaProducerConfig,
) -> Result<ClientConfig, String> {
    config.validate()?;

    let mut client = ClientConfig::new();
    client
        .set("bootstrap.servers", brokers)
        .set("acks", &config.acks)
        .set("enable.idempotence", config.enable_idempotence.to_string())
        .set(
            "queue.buffering.max.messages",
            config.queue_capacity_messages.to_string(),
        )
        .set("message.max.bytes", config.message_max_bytes.to_string())
        .set(
            "message.timeout.ms",
            config.message_timeout.as_millis().to_string(),
        )
        .set(
            "delivery.timeout.ms",
            config.delivery_timeout.as_millis().to_string(),
        )
        .set(
            "request.timeout.ms",
            config.request_timeout.as_millis().to_string(),
        );
    Ok(client)
}

fn consumer_client_config(
    brokers: &str,
    group_id: &str,
    config: &KafkaConsumerConfig,
) -> Result<ClientConfig, String> {
    config.validate()?;
    let receive_max_bytes = config
        .fetch_max_bytes
        .checked_add(KAFKA_RESPONSE_OVERHEAD_BYTES)
        .ok_or_else(|| "Kafka receive-message byte limit overflowed".to_string())?;

    let mut client = ClientConfig::new();
    client
        .set("bootstrap.servers", brokers)
        .set("group.id", group_id)
        .set("enable.auto.commit", "false")
        // Never advance librdkafka's stored position merely because recv returned a
        // record. Pulse commits only after a durable terminal disposition exists.
        .set("enable.auto.offset.store", "false")
        .set("enable.partition.eof", "false")
        .set("auto.offset.reset", "earliest")
        .set(
            "max.poll.interval.ms",
            config.max_poll_interval.as_millis().to_string(),
        )
        .set(
            "session.timeout.ms",
            config.session_timeout.as_millis().to_string(),
        )
        .set(
            "socket.timeout.ms",
            config.request_timeout.as_millis().to_string(),
        )
        .set(
            "queued.max.messages.kbytes",
            config.prefetch_kib.to_string(),
        )
        .set(
            "fetch.message.max.bytes",
            config.partition_fetch_max_bytes.to_string(),
        )
        .set("fetch.max.bytes", config.fetch_max_bytes.to_string())
        // Kafka may grow the per-partition fetch size to make progress on one
        // oversized record. Explicitly cap the protocol response as a final
        // guard on librdkafka's transient receive allocation.
        .set("receive.message.max.bytes", receive_max_bytes.to_string());
    Ok(client)
}

fn create_producer(brokers: &str, config: &KafkaProducerConfig) -> Result<FutureProducer, String> {
    producer_client_config(brokers, config)?
        .create()
        .map_err(|e| format!("failed to create Kafka producer: {e}"))
}

pub struct KafkaJobPublisher {
    producer: FutureProducer,
    topic: String,
    queue_timeout: Duration,
}

impl KafkaJobPublisher {
    /// Compatibility wrapper. New callers should use `new_with_config` so
    /// producer message counts are not confused with consumer prefetch KiB.
    pub fn new(
        brokers: &str,
        topic: impl Into<String>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        Self::new_with_config(
            brokers,
            topic,
            KafkaProducerConfig::with_queue_capacity(queue_capacity),
        )
    }

    pub fn new_with_config(
        brokers: &str,
        topic: impl Into<String>,
        config: KafkaProducerConfig,
    ) -> Result<Self, String> {
        let queue_timeout = config.delivery_timeout;
        let producer = create_producer(brokers, &config)?;

        Ok(Self {
            producer,
            topic: topic.into(),
            queue_timeout,
        })
    }
}

#[async_trait]
impl JobPublisher for KafkaJobPublisher {
    async fn publish_job(&self, key: &str, job: &ScenarioJob) -> Result<(), String> {
        let payload = serialize_validated_job(job)?;

        let started = Instant::now();
        let outcome = self
            .producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                self.queue_timeout,
            )
            .await
            .map_err(|(e, _)| format!("failed to publish job: {e}"));
        runtime_metrics::observe_kafka_publish("job", started.elapsed(), outcome.is_ok());
        outcome?;

        Ok(())
    }
}

pub struct KafkaResultPublisher {
    producer: FutureProducer,
    topic: String,
    queue_timeout: Duration,
}

impl KafkaResultPublisher {
    /// Compatibility wrapper. New callers should use `new_with_config`.
    pub fn new(
        brokers: &str,
        topic: impl Into<String>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        Self::new_with_config(
            brokers,
            topic,
            KafkaProducerConfig::with_queue_capacity(queue_capacity),
        )
    }

    pub fn new_with_config(
        brokers: &str,
        topic: impl Into<String>,
        config: KafkaProducerConfig,
    ) -> Result<Self, String> {
        let queue_timeout = config.delivery_timeout;
        let producer = create_producer(brokers, &config)?;

        Ok(Self {
            producer,
            topic: topic.into(),
            queue_timeout,
        })
    }
}

#[async_trait]
impl ResultPublisher for KafkaResultPublisher {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String> {
        let key = &result.execution_key;
        let payload = serialize_validated_result(result)?;

        let started = Instant::now();
        let outcome = self
            .producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                self.queue_timeout,
            )
            .await
            .map_err(|(e, _)| format!("failed to publish result: {e}"));
        runtime_metrics::observe_kafka_publish("result", started.elapsed(), outcome.is_ok());
        outcome?;

        Ok(())
    }
}

/// Publishes versioned aggregate revisions. All revisions for one run use the
/// run ID as their Kafka key, preserving per-run ordering; consumers dedupe the
/// deterministic `event_id` after crash-window republishes.
pub struct KafkaSummaryPublisher {
    producer: FutureProducer,
    topic: String,
    queue_timeout: Duration,
}

impl KafkaSummaryPublisher {
    pub fn new_with_config(
        brokers: &str,
        topic: impl Into<String>,
        config: KafkaProducerConfig,
    ) -> Result<Self, String> {
        let queue_timeout = config.delivery_timeout;
        let producer = create_producer(brokers, &config)?;
        Ok(Self {
            producer,
            topic: topic.into(),
            queue_timeout,
        })
    }
}

#[async_trait]
impl SummaryPublisher for KafkaSummaryPublisher {
    async fn publish_summary(&self, event: &ScenarioRunSummaryEvent) -> Result<(), String> {
        let payload = serde_json::to_string(event)
            .map_err(|error| format!("failed to serialize run summary: {error}"))?;
        let started = Instant::now();
        let outcome = self
            .producer
            .send(
                FutureRecord::to(&self.topic)
                    .key(&event.summary.run_id)
                    .payload(&payload),
                self.queue_timeout,
            )
            .await
            .map_err(|(error, _)| format!("failed to publish run summary: {error}"));
        runtime_metrics::observe_kafka_publish("summary", started.elapsed(), outcome.is_ok());
        outcome.map(|_| ())
    }
}

pub struct KafkaJobConsumer {
    consumer: Arc<FencedStreamConsumer>,
    record_max_bytes: usize,
}

pub struct KafkaResultConsumer {
    consumer: Arc<FencedStreamConsumer>,
    record_max_bytes: usize,
}

impl KafkaResultConsumer {
    pub fn new_with_config(
        brokers: &str,
        group_id: &str,
        topic: &str,
        config: KafkaConsumerConfig,
    ) -> Result<Self, String> {
        let consumer: FencedStreamConsumer = consumer_client_config(brokers, group_id, &config)?
            .create_with_context(RebalanceFence::default())
            .map_err(|error| format!("failed to create Kafka result consumer: {error}"))?;
        consumer
            .subscribe(&[topic])
            .map_err(|error| format!("failed to subscribe result topic: {error}"))?;
        Ok(Self {
            consumer: Arc::new(consumer),
            record_max_bytes: config.record_max_bytes,
        })
    }
}

impl KafkaJobConsumer {
    /// Compatibility wrapper. New callers should use `new_with_config` so
    /// consumer prefetch KiB are configured independently from producer counts.
    pub fn new(
        brokers: &str,
        group_id: &str,
        topic: &str,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        Self::new_with_config(
            brokers,
            group_id,
            topic,
            KafkaConsumerConfig::with_prefetch_kib(queue_capacity),
        )
    }

    pub fn new_with_config(
        brokers: &str,
        group_id: &str,
        topic: &str,
        config: KafkaConsumerConfig,
    ) -> Result<Self, String> {
        let consumer: FencedStreamConsumer = consumer_client_config(brokers, group_id, &config)?
            .create_with_context(RebalanceFence::default())
            .map_err(|e| format!("failed to create Kafka consumer: {e}"))?;

        consumer
            .subscribe(&[topic])
            .map_err(|e| format!("failed to subscribe topic: {e}"))?;

        Ok(Self {
            consumer: Arc::new(consumer),
            record_max_bytes: config.record_max_bytes,
        })
    }

    /// Assignment epoch used by broker-backed recovery tests and diagnostics.
    /// A consumed record may commit only while this value is unchanged.
    pub fn rebalance_epoch(&self) -> u64 {
        self.consumer.context().epoch()
    }
}

pub struct KafkaDlqPublisher {
    producer: FutureProducer,
    topic: String,
    queue_timeout: Duration,
}

impl KafkaDlqPublisher {
    /// Compatibility wrapper. New callers should use `new_with_config`.
    pub fn new(
        brokers: &str,
        topic: impl Into<String>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        Self::new_with_config(
            brokers,
            topic,
            KafkaProducerConfig::with_queue_capacity(queue_capacity),
        )
    }

    pub fn new_with_config(
        brokers: &str,
        topic: impl Into<String>,
        config: KafkaProducerConfig,
    ) -> Result<Self, String> {
        let queue_timeout = config.delivery_timeout;
        let producer = create_producer(brokers, &config)?;

        Ok(Self {
            producer,
            topic: topic.into(),
            queue_timeout,
        })
    }
}

#[async_trait]
impl DlqPublisher for KafkaDlqPublisher {
    async fn publish_failed_job(&self, key: &str, job: &FailedScenarioJob) -> Result<(), String> {
        if key != job.execution_key {
            return Err("refusing to publish failed job under a non-execution key".to_string());
        }
        let payload = serialize_validated_failed_job(job)?;

        let started = Instant::now();
        let outcome = self
            .producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                self.queue_timeout,
            )
            .await
            .map_err(|(e, _)| format!("failed to publish dead-letter job: {e}"));
        runtime_metrics::observe_kafka_publish("dlq", started.elapsed(), outcome.is_ok());
        outcome?;

        Ok(())
    }

    async fn publish_poison(&self, record: &PoisonMessageRecord) -> Result<(), String> {
        let payload = serialize_validated_poison(record)?;

        let started = Instant::now();
        let outcome = self
            .producer
            .send(
                FutureRecord::to(&self.topic)
                    .key(&record.event_id)
                    .payload(&payload),
                self.queue_timeout,
            )
            .await
            .map_err(|(e, _)| format!("failed to publish poison message: {e}"));
        runtime_metrics::observe_kafka_publish("poison", started.elapsed(), outcome.is_ok());
        outcome?;

        Ok(())
    }
}

/// A Kafka record whose source offset remains unsettled until `commit` is called.
///
/// Deserialization errors are retained in the record instead of being returned by
/// `JobConsumer::recv`, allowing the worker to publish a deterministic poison DLQ
/// record before settling the source offset.
pub struct ConsumedJob {
    topic: String,
    partition: i32,
    offset: i64,
    source_key: BoundedPoisonEvidence,
    payload: BoundedPoisonEvidence,
    job: Result<ScenarioJob, ContractError>,
    consumer: Arc<FencedStreamConsumer>,
    rebalance_epoch: u64,
}

pub struct ConsumedResult {
    topic: String,
    partition: i32,
    offset: i64,
    source_key: BoundedPoisonEvidence,
    payload: BoundedPoisonEvidence,
    result: Result<ScenarioRunResult, ContractError>,
    consumer: Arc<FencedStreamConsumer>,
    rebalance_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BoundedPoisonEvidence {
    prefix: Option<Vec<u8>>,
    original_bytes: Option<u64>,
    truncated: bool,
}

impl BoundedPoisonEvidence {
    fn capture(value: Option<&[u8]>) -> Self {
        let Some(value) = value else {
            return Self {
                prefix: None,
                original_bytes: None,
                truncated: false,
            };
        };
        let retained_bytes = value.len().min(MAX_POISON_EVIDENCE_FIELD_BYTES);
        Self {
            prefix: Some(value[..retained_bytes].to_vec()),
            original_bytes: Some(u64::try_from(value.len()).unwrap_or(u64::MAX)),
            truncated: value.len() > retained_bytes,
        }
    }
}

fn consumed_record_size(
    source_key: Option<&[u8]>,
    payload: Option<&[u8]>,
) -> Result<usize, ContractError> {
    let key_bytes = source_key.map_or(0, <[u8]>::len);
    let payload_bytes = payload.map_or(0, <[u8]>::len);
    key_bytes.checked_add(payload_bytes).ok_or_else(|| {
        ContractError::Malformed(format!(
            "Kafka record byte length overflowed (key={key_bytes}, payload={payload_bytes})"
        ))
    })
}

fn enforce_consumed_record_size(
    source_key: Option<&[u8]>,
    payload: Option<&[u8]>,
    record_max_bytes: usize,
) -> Result<(), ContractError> {
    let observed = consumed_record_size(source_key, payload)?;
    if observed > record_max_bytes {
        let key_bytes = source_key.map_or(0, <[u8]>::len);
        let payload_bytes = payload.map_or(0, <[u8]>::len);
        return Err(ContractError::Malformed(format!(
            "Kafka record exceeds configured {record_max_bytes}-byte consumer limit (total={observed}, key={key_bytes}, payload={payload_bytes})"
        )));
    }
    Ok(())
}

fn decode_consumed_job(
    source_key: Option<&[u8]>,
    payload: Option<&[u8]>,
    record_max_bytes: usize,
) -> Result<ScenarioJob, ContractError> {
    enforce_consumed_record_size(source_key, payload, record_max_bytes)?;
    decode_job(payload)
}

fn decode_consumed_result(
    source_key: Option<&[u8]>,
    payload: Option<&[u8]>,
    record_max_bytes: usize,
) -> Result<ScenarioRunResult, ContractError> {
    enforce_consumed_record_size(source_key, payload, record_max_bytes)?;
    decode_result(payload)
}

fn decode_job(payload: Option<&[u8]>) -> Result<ScenarioJob, ContractError> {
    let payload = payload
        .ok_or_else(|| ContractError::Malformed("Kafka message has no payload".to_string()))?;
    let job: ScenarioJob = serde_json::from_slice(payload)
        .map_err(|err| ContractError::Malformed(format!("invalid scenario job JSON: {err}")))?;
    job.validate()?;
    Ok(job)
}

fn decode_result(payload: Option<&[u8]>) -> Result<ScenarioRunResult, ContractError> {
    let payload = payload
        .ok_or_else(|| ContractError::Malformed("Kafka result has no payload".to_string()))?;
    let result: ScenarioRunResult = serde_json::from_slice(payload).map_err(|error| {
        ContractError::Malformed(format!("invalid scenario result JSON: {error}"))
    })?;
    result.validate()?;
    Ok(result)
}

fn serialize_validated_job(job: &ScenarioJob) -> Result<String, String> {
    job.validate()
        .map_err(|error| format!("refusing to publish invalid scenario job: {error}"))?;
    serde_json::to_string(job).map_err(|error| format!("failed to serialize scenario job: {error}"))
}

fn serialize_validated_result(result: &ScenarioRunResult) -> Result<String, String> {
    result
        .validate()
        .map_err(|error| format!("refusing to publish invalid scenario result: {error}"))?;
    serde_json::to_string(result)
        .map_err(|error| format!("failed to serialize scenario result: {error}"))
}

fn serialize_validated_failed_job(job: &FailedScenarioJob) -> Result<String, String> {
    let mut job = job.clone();
    job.reason = bounded_dlq_reason(job.reason);
    validate_contract_version(job.schema_version)
        .map_err(|error| format!("refusing to publish invalid failed job: {error}"))?;
    for (field, value) in [
        ("scenario_id", job.scenario_id.as_str()),
        ("run_id", job.run_id.as_str()),
        ("execution_key", job.execution_key.as_str()),
        ("reason", job.reason.as_str()),
    ] {
        validate_dlq_text(field, value)?;
    }
    validate_dlq_slice(&job.slice)?;
    if job.attempt > job.max_retries || job.max_retries > MAX_CONTRACT_ATTEMPT {
        return Err("refusing to publish failed job with invalid attempt metadata".to_string());
    }
    if job.schema_version >= 2 {
        validate_dlq_text("event_id", &job.event_id)?;
        if job.event_id != build_terminal_event_id(&job.execution_key, job.attempt, "dlq") {
            return Err(
                "refusing to publish failed job with a non-deterministic event_id".to_string(),
            );
        }
    }
    serde_json::to_string(&job)
        .map_err(|error| format!("failed to serialize failed scenario job: {error}"))
}

fn serialize_validated_poison(record: &PoisonMessageRecord) -> Result<String, String> {
    let mut record = record.clone();
    record.reason = bounded_dlq_reason(record.reason);
    validate_contract_version(record.schema_version)
        .map_err(|error| format!("refusing to publish invalid poison record: {error}"))?;
    for (field, value) in [
        ("event_id", record.event_id.as_str()),
        ("source_topic", record.source_topic.as_str()),
        ("reason", record.reason.as_str()),
    ] {
        validate_dlq_text(field, value)?;
    }
    if record.source_partition < 0 || record.source_offset < 0 {
        return Err(
            "refusing to publish poison record with invalid source coordinates".to_string(),
        );
    }
    if record.event_id
        != format!(
            "poison:{}:{}:{}",
            record.source_topic, record.source_partition, record.source_offset
        )
    {
        return Err(
            "refusing to publish poison record with a non-deterministic event_id".to_string(),
        );
    }
    validate_poison_evidence_for_publish(
        record.schema_version,
        "source_key",
        record.source_key_base64.as_deref(),
        record.source_key_original_bytes,
        record.source_key_truncated,
    )?;
    validate_poison_evidence_for_publish(
        record.schema_version,
        "payload",
        record.payload_base64.as_deref(),
        record.payload_original_bytes,
        record.payload_truncated,
    )?;
    serde_json::to_string(&record)
        .map_err(|error| format!("failed to serialize poison message record: {error}"))
}

fn validate_dlq_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_CONTRACT_ID_BYTES {
        return Err(format!(
            "refusing to publish DLQ record with invalid {field}"
        ));
    }
    Ok(())
}

fn validate_dlq_slice(slice: &crate::domain::contracts::JobSlice) -> Result<(), String> {
    if slice.total == 0 || slice.total > MAX_CONTRACT_SLICES || slice.index >= slice.total {
        return Err("refusing to publish failed job with invalid slice metadata".to_string());
    }
    Ok(())
}

fn validate_poison_evidence_for_publish(
    schema_version: u16,
    field: &str,
    encoded: Option<&str>,
    original_bytes: Option<u64>,
    truncated: bool,
) -> Result<(), String> {
    let Some(encoded) = encoded else {
        if original_bytes.is_some() || truncated {
            return Err(format!("invalid poison {field} evidence metadata"));
        }
        return Ok(());
    };
    let max_encoded_bytes = MAX_POISON_EVIDENCE_FIELD_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded_bytes {
        return Err(format!("poison {field} evidence exceeds its prefix bound"));
    }
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid poison {field} base64: {error}"))?;
    let retained_bytes = u64::try_from(decoded.len()).unwrap_or(u64::MAX);
    if decoded.len() > MAX_POISON_EVIDENCE_FIELD_BYTES {
        return Err(format!("poison {field} evidence exceeds its prefix bound"));
    }
    let Some(original_bytes) = original_bytes else {
        return if schema_version < 2 && !truncated {
            Ok(())
        } else {
            Err(format!(
                "poison {field} evidence lacks its original byte count"
            ))
        };
    };
    if (truncated && original_bytes <= retained_bytes)
        || (!truncated && original_bytes != retained_bytes)
    {
        return Err(format!(
            "poison {field} evidence length metadata is inconsistent"
        ));
    }
    Ok(())
}

#[cfg(test)]
fn poison_record(
    topic: &str,
    partition: i32,
    offset: i64,
    source_key: Option<&[u8]>,
    payload: Option<&[u8]>,
    reason: String,
) -> PoisonMessageRecord {
    let source_key = BoundedPoisonEvidence::capture(source_key);
    let payload = BoundedPoisonEvidence::capture(payload);
    poison_record_from_evidence(topic, partition, offset, &source_key, &payload, reason)
}

fn poison_record_from_evidence(
    topic: &str,
    partition: i32,
    offset: i64,
    source_key: &BoundedPoisonEvidence,
    payload: &BoundedPoisonEvidence,
    reason: String,
) -> PoisonMessageRecord {
    PoisonMessageRecord {
        schema_version: CURRENT_CONTRACT_VERSION,
        event_id: format!("poison:{topic}:{partition}:{offset}"),
        failed_at_unix_ms: now_unix_ms(),
        source_topic: topic.to_string(),
        source_partition: partition,
        source_offset: offset,
        source_key_base64: source_key
            .prefix
            .as_deref()
            .map(|prefix| BASE64_STANDARD.encode(prefix)),
        source_key_original_bytes: source_key.original_bytes,
        source_key_truncated: source_key.truncated,
        payload_base64: payload
            .prefix
            .as_deref()
            .map(|prefix| BASE64_STANDARD.encode(prefix)),
        payload_original_bytes: payload.original_bytes,
        payload_truncated: payload.truncated,
        reason: bounded_dlq_reason(reason),
    }
}

fn bounded_dlq_reason(mut reason: String) -> String {
    if reason.len() <= MAX_CONTRACT_ID_BYTES {
        return reason;
    }

    let mut keep = MAX_CONTRACT_ID_BYTES - POISON_REASON_TRUNCATION_MARKER.len();
    while !reason.is_char_boundary(keep) {
        keep -= 1;
    }
    reason.truncate(keep);
    reason.push_str(POISON_REASON_TRUNCATION_MARKER);
    reason
}

#[async_trait]
impl JobConsumer for KafkaJobConsumer {
    type Item = ConsumedJob;

    async fn recv(&self) -> Result<Option<Self::Item>, String> {
        let msg = self
            .consumer
            .recv()
            .await
            .map_err(|e| format!("Kafka receive error: {e}"))?;

        let topic = msg.topic().to_string();
        let partition = msg.partition();
        let offset = msg.offset();
        let source_key = BoundedPoisonEvidence::capture(msg.key());
        let payload = BoundedPoisonEvidence::capture(msg.payload());
        let job = decode_consumed_job(msg.key(), msg.payload(), self.record_max_bytes);
        let rebalance_epoch = self.consumer.context().epoch();

        Ok(Some(ConsumedJob {
            topic,
            partition,
            offset,
            source_key,
            payload,
            job,
            consumer: self.consumer.clone(),
            rebalance_epoch,
        }))
    }
}

#[async_trait]
impl ResultConsumer for KafkaResultConsumer {
    type Item = ConsumedResult;

    async fn recv(&self) -> Result<Option<Self::Item>, String> {
        let msg = self
            .consumer
            .recv()
            .await
            .map_err(|error| format!("Kafka result receive error: {error}"))?;
        let topic = msg.topic().to_string();
        let partition = msg.partition();
        let offset = msg.offset();
        let source_key = BoundedPoisonEvidence::capture(msg.key());
        let payload = BoundedPoisonEvidence::capture(msg.payload());
        let result = decode_consumed_result(msg.key(), msg.payload(), self.record_max_bytes);
        let rebalance_epoch = self.consumer.context().epoch();
        Ok(Some(ConsumedResult {
            topic,
            partition,
            offset,
            source_key,
            payload,
            result,
            consumer: self.consumer.clone(),
            rebalance_epoch,
        }))
    }
}

impl ConsumedJob {
    async fn commit_inner(self) -> Result<(), String> {
        commit_offset(
            self.consumer,
            self.rebalance_epoch,
            self.topic,
            self.partition,
            self.offset,
        )
        .await
    }
}

async fn commit_offset(
    consumer: Arc<FencedStreamConsumer>,
    consumed_epoch: u64,
    topic: String,
    partition: i32,
    offset: i64,
) -> Result<(), String> {
    let started = Instant::now();
    let next_offset = offset
        .checked_add(1)
        .ok_or_else(|| "cannot commit Kafka offset because it overflowed".to_string())?;
    let outcome = tokio::task::spawn_blocking(move || {
        let current_epoch = consumer.context().epoch();
        if current_epoch != consumed_epoch {
            return Err(format!(
                "refusing Kafka offset commit after rebalance: consumed epoch {consumed_epoch}, current epoch {current_epoch}"
            ));
        }
        let assignment = consumer
            .assignment()
            .map_err(|error| format!("failed to inspect Kafka assignment before commit: {error}"))?;
        if assignment.find_partition(&topic, partition).is_none() {
            return Err(format!(
                "refusing Kafka offset commit for revoked assignment {topic}[{partition}]"
            ));
        }
        let mut offsets = TopicPartitionList::new();
        offsets
            .add_partition_offset(&topic, partition, Offset::Offset(next_offset))
            .map_err(|error| format!("failed to build Kafka commit offset: {error}"))?;
        // CommitMode::Sync does not return until librdkafka receives the broker's
        // offset-commit response. Run it off the Tokio executor because it blocks.
        let committed = consumer
            .commit(&offsets, CommitMode::Sync)
            .map_err(|error| format!("failed to commit Kafka message: {error}"));
        let current_epoch = consumer.context().epoch();
        if current_epoch != consumed_epoch {
            return Err(format!(
                "Kafka rebalance raced with offset commit acknowledgement: consumed epoch {consumed_epoch}, current epoch {current_epoch}"
            ));
        }
        let assignment = consumer
            .assignment()
            .map_err(|error| format!("failed to inspect Kafka assignment after commit: {error}"))?;
        if assignment.find_partition(&topic, partition).is_none() {
            return Err(format!(
                "Kafka assignment {topic}[{partition}] was revoked during commit acknowledgement"
            ));
        }
        committed
    })
    .await
    .map_err(|error| format!("Kafka commit task failed: {error}"))?;
    runtime_metrics::observe_kafka_commit(started.elapsed());
    outcome
}

#[async_trait]
impl CommitableJob for ConsumedJob {
    fn job(&self) -> Result<&ScenarioJob, ContractError> {
        match &self.job {
            Ok(job) => Ok(job),
            Err(err) => Err(err.clone()),
        }
    }

    fn poison_record(&self, reason: String) -> PoisonMessageRecord {
        poison_record_from_evidence(
            &self.topic,
            self.partition,
            self.offset,
            &self.source_key,
            &self.payload,
            reason,
        )
    }

    fn source_topic(&self) -> Option<&str> {
        Some(&self.topic)
    }

    fn source_partition(&self) -> Option<i32> {
        Some(self.partition)
    }

    fn source_offset(&self) -> Option<i64> {
        Some(self.offset)
    }

    async fn commit(self) -> Result<(), String> {
        self.commit_inner().await
    }
}

#[async_trait]
impl CommitableResult for ConsumedResult {
    fn result(&self) -> Result<&ScenarioRunResult, ContractError> {
        match &self.result {
            Ok(result) => Ok(result),
            Err(error) => Err(error.clone()),
        }
    }

    fn poison_record(&self, reason: String) -> PoisonMessageRecord {
        poison_record_from_evidence(
            &self.topic,
            self.partition,
            self.offset,
            &self.source_key,
            &self.payload,
            reason,
        )
    }

    async fn commit(self) -> Result<(), String> {
        commit_offset(
            self.consumer,
            self.rebalance_epoch,
            self.topic,
            self.partition,
            self.offset,
        )
        .await
    }
}

pub async fn ensure_topics(brokers: &str, topics: &[(&str, i32, i32)]) -> Result<(), String> {
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .map_err(|e| format!("failed to create Kafka admin client: {e}"))?;

    let new_topics: Vec<_> = topics
        .iter()
        .map(|(name, partitions, replication)| {
            NewTopic::new(
                name,
                *partitions,
                TopicReplication::Fixed((*replication).max(1)),
            )
        })
        .collect();

    let results = admin
        .create_topics(&new_topics, &AdminOptions::new())
        .await
        .map_err(|e| format!("failed to create Kafka topics: {e}"))?;

    for result in results {
        match result {
            Ok(_) => {}
            Err((_, RDKafkaErrorCode::TopicAlreadyExists)) => {}
            Err((topic, code)) => {
                return Err(format!("failed to ensure Kafka topic {topic}: {code:?}"));
            }
        }
    }

    Ok(())
}

/// Verifies that at least one configured broker can return cluster metadata.
///
/// Producer/consumer construction is local-only in librdkafka, so successful
/// constructors are not sufficient evidence for readiness. The blocking
/// metadata request is kept off the Tokio executor.
pub async fn probe_brokers(brokers: &str, probe_timeout: Duration) -> Result<(), String> {
    let brokers = brokers.to_owned();
    tokio::task::spawn_blocking(move || {
        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .create()
            .map_err(|error| format!("failed to create Kafka readiness client: {error}"))?;
        let metadata = consumer
            .fetch_metadata(None, probe_timeout)
            .map_err(|error| format!("Kafka metadata probe failed: {error}"))?;
        if metadata.brokers().is_empty() {
            return Err("Kafka metadata probe returned no brokers".to_string());
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("Kafka metadata probe task failed: {error}"))?
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TopicReadinessMetadata {
    name: String,
    error: Option<String>,
    partitions: Vec<PartitionReadinessMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartitionReadinessMetadata {
    id: i32,
    leader: i32,
    error: Option<String>,
}

/// Verifies that each required topic exists and every advertised partition has
/// usable metadata. The probe is read-only: explicit topic metadata requests
/// disable Kafka's automatic topic creation behavior.
///
/// One total timeout is shared across all requests, so a larger required-topic
/// set cannot multiply the readiness deadline. Pulse currently passes its four
/// bounded runtime topics (jobs, results, summaries, and DLQ).
pub async fn probe_required_topics(
    brokers: &str,
    required_topics: &[String],
    probe_timeout: Duration,
) -> Result<(), String> {
    let brokers = brokers.to_owned();
    let required_topics = required_topics.to_vec();
    tokio::task::spawn_blocking(move || {
        if required_topics.is_empty() {
            return Err("Kafka required-topic probe received no topics".to_string());
        }

        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            // A readiness probe must never create infrastructure as a side
            // effect, even when the broker permits automatic topic creation.
            .set("allow.auto.create.topics", "false")
            .create()
            .map_err(|error| format!("failed to create Kafka topic readiness client: {error}"))?;

        let started = Instant::now();
        let mut observed = Vec::with_capacity(required_topics.len());
        for required_topic in &required_topics {
            let remaining = probe_timeout
                .checked_sub(started.elapsed())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| {
                    format!(
                        "Kafka required-topic probe exceeded its {} ms deadline",
                        probe_timeout.as_millis()
                    )
                })?;
            let metadata = consumer
                .fetch_metadata(Some(required_topic), remaining)
                .map_err(|error| {
                    format!("Kafka metadata probe for topic '{required_topic}' failed: {error}")
                })?;
            if metadata.brokers().is_empty() {
                return Err(format!(
                    "Kafka metadata probe for topic '{required_topic}' returned no brokers"
                ));
            }
            observed.extend(
                metadata
                    .topics()
                    .iter()
                    .filter(|topic| topic.name() == required_topic)
                    .map(|topic| TopicReadinessMetadata {
                        name: topic.name().to_string(),
                        error: topic.error().map(|error| format!("{error:?}")),
                        partitions: topic
                            .partitions()
                            .iter()
                            .map(|partition| PartitionReadinessMetadata {
                                id: partition.id(),
                                leader: partition.leader(),
                                error: partition.error().map(|error| format!("{error:?}")),
                            })
                            .collect(),
                    }),
            );
        }

        validate_required_topic_metadata(&required_topics, &observed)
    })
    .await
    .map_err(|error| format!("Kafka required-topic probe task failed: {error}"))?
}

fn validate_required_topic_metadata(
    required_topics: &[String],
    observed: &[TopicReadinessMetadata],
) -> Result<(), String> {
    for (index, required) in required_topics.iter().enumerate() {
        if required_topics[..index].contains(required) {
            return Err(format!("duplicate Kafka required topic '{required}'"));
        }

        let matching: Vec<_> = observed
            .iter()
            .filter(|topic| topic.name == *required)
            .collect();
        let topic = match matching.as_slice() {
            [] => return Err(format!("required Kafka topic '{required}' is missing")),
            [topic] => *topic,
            _ => {
                return Err(format!(
                    "Kafka metadata contained duplicate entries for required topic '{required}'"
                ));
            }
        };
        if let Some(error) = &topic.error {
            return Err(format!(
                "required Kafka topic '{required}' has metadata error {error}"
            ));
        }
        if topic.partitions.is_empty() {
            return Err(format!(
                "required Kafka topic '{required}' has no partitions"
            ));
        }
        for (partition_index, partition) in topic.partitions.iter().enumerate() {
            if partition.id < 0 {
                return Err(format!(
                    "required Kafka topic '{required}' has invalid partition id {}",
                    partition.id
                ));
            }
            if topic.partitions[..partition_index]
                .iter()
                .any(|existing| existing.id == partition.id)
            {
                return Err(format!(
                    "required Kafka topic '{required}' has duplicate partition id {}",
                    partition.id
                ));
            }
            if let Some(error) = &partition.error {
                return Err(format!(
                    "required Kafka topic '{required}' partition {} has metadata error {error}",
                    partition.id
                ));
            }
            if partition.leader < 0 {
                return Err(format!(
                    "required Kafka topic '{required}' partition {} has no leader",
                    partition.id
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::contracts::{
        JobLoadConfig, JobSlice, MAX_CONTRACT_ATTEMPT, ScenarioRunStatus, build_terminal_event_id,
    };

    fn valid_job() -> ScenarioJob {
        ScenarioJob {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "checkout".to_string(),
            run_id: "run-1".to_string(),
            execution_key: "execution-1".to_string(),
            plan_fingerprint: "fnv128:kafka-test-plan".to_string(),
            scheduled_at_unix_ms: 1,
            not_before_unix_ms: 0,
            slice: JobSlice { index: 0, total: 1 },
            load: JobLoadConfig {
                scenarios_per_sec: 0.1,
                duration: Duration::from_secs(1),
                max_concurrency: 1,
                startup_burst: 0,
            },
            attempt: 0,
            max_retries: 2,
        }
    }

    fn valid_result() -> ScenarioRunResult {
        let execution_key = "execution-1".to_string();
        ScenarioRunResult {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "checkout".to_string(),
            run_id: "run-1".to_string(),
            execution_key: execution_key.clone(),
            event_id: build_terminal_event_id(&execution_key, 0, "result"),
            attempt: 0,
            slice: JobSlice { index: 0, total: 1 },
            started_at_unix_ms: 1,
            finished_at_unix_ms: 2,
            status: ScenarioRunStatus::Success,
            total: 0,
            success: 0,
            failure: 0,
            scenario_latency_p50_ms: 0,
            scenario_latency_p95_ms: 0,
            scenario_latency_p99_ms: 0,
            latency_histogram: Vec::new(),
            error_breakdown: Vec::new(),
        }
    }

    fn valid_failed_job() -> FailedScenarioJob {
        let execution_key = "execution-1".to_string();
        FailedScenarioJob {
            schema_version: CURRENT_CONTRACT_VERSION,
            event_id: build_terminal_event_id(&execution_key, 0, "dlq"),
            scenario_id: "checkout".to_string(),
            run_id: "run-1".to_string(),
            execution_key,
            slice: JobSlice { index: 0, total: 1 },
            failed_at_unix_ms: 2,
            attempt: 0,
            max_retries: 2,
            reason: "permanent processing failure".to_string(),
        }
    }

    fn ready_topic(name: &str, partitions: usize) -> TopicReadinessMetadata {
        TopicReadinessMetadata {
            name: name.to_string(),
            error: None,
            partitions: (0..partitions)
                .map(|id| PartitionReadinessMetadata {
                    id: i32::try_from(id).expect("test partition fits i32"),
                    leader: 1,
                    error: None,
                })
                .collect(),
        }
    }

    #[test]
    fn required_topic_metadata_accepts_only_present_usable_partitions() {
        let required = vec!["jobs".to_string(), "results".to_string()];
        let observed = vec![ready_topic("jobs", 3), ready_topic("results", 1)];
        assert_eq!(
            validate_required_topic_metadata(&required, &observed),
            Ok(())
        );
    }

    #[test]
    fn required_topic_metadata_fails_closed_for_missing_or_unusable_topics() {
        let required = vec!["jobs".to_string(), "results".to_string()];

        let error = validate_required_topic_metadata(&required, &[ready_topic("jobs", 1)])
            .expect_err("missing results topic must fail readiness");
        assert!(error.contains("'results' is missing"));

        let error = validate_required_topic_metadata(
            &required,
            &[ready_topic("jobs", 1), ready_topic("results", 0)],
        )
        .expect_err("partitionless topic must fail readiness");
        assert!(error.contains("has no partitions"));

        let mut leaderless = ready_topic("results", 1);
        leaderless.partitions[0].leader = -1;
        let error =
            validate_required_topic_metadata(&required, &[ready_topic("jobs", 1), leaderless])
                .expect_err("leaderless partition must fail readiness");
        assert!(error.contains("has no leader"));

        let mut partition_error = ready_topic("results", 1);
        partition_error.partitions[0].error = Some("LeaderNotAvailable".to_string());
        let error =
            validate_required_topic_metadata(&required, &[ready_topic("jobs", 1), partition_error])
                .expect_err("partition error must fail readiness");
        assert!(error.contains("LeaderNotAvailable"));
    }

    #[test]
    fn producer_configuration_is_durable_and_count_bounded() {
        let config = KafkaProducerConfig {
            queue_capacity_messages: 321,
            message_max_bytes: 765_432,
            message_timeout: Duration::from_secs(9),
            delivery_timeout: Duration::from_secs(9),
            request_timeout: Duration::from_secs(4),
            acks: "all".to_string(),
            enable_idempotence: true,
        };

        let client = producer_client_config("broker:9092", &config).unwrap();
        assert_eq!(client.get("acks"), Some("all"));
        assert_eq!(client.get("enable.idempotence"), Some("true"));
        assert_eq!(client.get("queue.buffering.max.messages"), Some("321"));
        assert_eq!(client.get("message.max.bytes"), Some("765432"));
        assert_eq!(client.get("message.timeout.ms"), Some("9000"));
        assert_eq!(client.get("delivery.timeout.ms"), Some("9000"));
        assert_eq!(client.get("request.timeout.ms"), Some("4000"));
    }

    #[test]
    fn producer_rejects_settings_that_weaken_durable_publication() {
        let config = KafkaProducerConfig {
            acks: "1".to_string(),
            ..KafkaProducerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = KafkaProducerConfig {
            delivery_timeout: Duration::from_secs(11),
            ..KafkaProducerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = KafkaProducerConfig {
            request_timeout: Duration::from_secs(11),
            ..KafkaProducerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn consumer_disables_automatic_offset_storage_and_sets_poll_limits() {
        let config = KafkaConsumerConfig {
            max_poll_interval: Duration::from_secs(180),
            session_timeout: Duration::from_secs(12),
            request_timeout: Duration::from_secs(4),
            prefetch_kib: 2_048,
            partition_fetch_max_bytes: 786_432,
            fetch_max_bytes: 6_291_456,
            record_max_bytes: 1_500_000,
        };

        let client = consumer_client_config("broker:9092", "workers", &config).unwrap();
        assert_eq!(client.get("enable.auto.commit"), Some("false"));
        assert_eq!(client.get("enable.auto.offset.store"), Some("false"));
        assert_eq!(client.get("max.poll.interval.ms"), Some("180000"));
        assert_eq!(client.get("session.timeout.ms"), Some("12000"));
        assert_eq!(client.get("socket.timeout.ms"), Some("4000"));
        assert_eq!(client.get("queued.max.messages.kbytes"), Some("2048"));
        assert_eq!(client.get("fetch.message.max.bytes"), Some("786432"));
        assert_eq!(client.get("fetch.max.bytes"), Some("6291456"));
        assert_eq!(client.get("receive.message.max.bytes"), Some("6291968"));

        let invalid = KafkaConsumerConfig {
            partition_fetch_max_bytes: 2_000,
            fetch_max_bytes: 1_999,
            ..KafkaConsumerConfig::default()
        };
        assert!(invalid.validate().is_err());

        let invalid = KafkaConsumerConfig {
            record_max_bytes: 6_291_457,
            ..config
        };
        assert!(invalid.validate().is_err());

        let invalid = KafkaConsumerConfig {
            record_max_bytes: MAX_POISON_EVIDENCE_FIELD_BYTES * 2 - 1,
            ..KafkaConsumerConfig::default()
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn malformed_or_missing_payload_is_retained_as_contract_error() {
        assert!(matches!(
            decode_job(None),
            Err(ContractError::Malformed(message)) if message.contains("no payload")
        ));
        assert!(matches!(
            decode_job(Some(b"{not-json")),
            Err(ContractError::Malformed(message)) if message.contains("invalid scenario job JSON")
        ));

        let payload = serde_json::to_vec(&valid_job()).unwrap();
        assert_eq!(
            decode_job(Some(&payload)).unwrap().execution_key,
            "execution-1"
        );
    }

    #[test]
    fn consumed_record_limit_counts_key_and_payload_before_decoding() {
        let payload = serde_json::to_vec(&valid_job()).expect("job serializes");
        let key = b"execution-1";
        let exact_limit = key.len() + payload.len();

        assert!(decode_consumed_job(Some(key), Some(&payload), exact_limit).is_ok());
        let error = decode_consumed_job(Some(key), Some(&payload), exact_limit - 1)
            .expect_err("combined key and payload above the limit must be poison");
        assert!(matches!(
            error,
            ContractError::Malformed(message)
                if message.contains("exceeds configured")
                    && message.contains(&format!("total={exact_limit}"))
        ));
    }

    #[test]
    fn oversized_record_retains_only_bounded_prefixes_and_exact_lengths() {
        let key = vec![0x11; MAX_POISON_EVIDENCE_FIELD_BYTES + 17];
        let payload = vec![0x22; MAX_POISON_EVIDENCE_FIELD_BYTES + 29];
        let key_evidence = BoundedPoisonEvidence::capture(Some(&key));
        let payload_evidence = BoundedPoisonEvidence::capture(Some(&payload));
        let record = poison_record_from_evidence(
            "pulse.jobs",
            2,
            19,
            &key_evidence,
            &payload_evidence,
            "record exceeded configured consumer limit".to_string(),
        );

        assert_eq!(
            key_evidence.prefix.as_ref().map(Vec::len),
            Some(MAX_POISON_EVIDENCE_FIELD_BYTES)
        );
        assert_eq!(
            payload_evidence.prefix.as_ref().map(Vec::len),
            Some(MAX_POISON_EVIDENCE_FIELD_BYTES)
        );
        assert_eq!(
            record.source_key_original_bytes,
            Some(u64::try_from(key.len()).expect("test length fits u64"))
        );
        assert_eq!(
            record.payload_original_bytes,
            Some(u64::try_from(payload.len()).expect("test length fits u64"))
        );
        assert!(record.source_key_truncated);
        assert!(record.payload_truncated);
        assert_eq!(
            BASE64_STANDARD
                .decode(record.source_key_base64.as_deref().expect("key evidence"))
                .expect("valid key base64")
                .len(),
            MAX_POISON_EVIDENCE_FIELD_BYTES
        );
        assert_eq!(
            BASE64_STANDARD
                .decode(record.payload_base64.as_deref().expect("payload evidence"))
                .expect("valid payload base64")
                .len(),
            MAX_POISON_EVIDENCE_FIELD_BYTES
        );
        assert!(serialize_validated_poison(&record).is_ok());
    }

    #[test]
    fn decoded_kafka_contracts_are_validated_before_processing() {
        let mut future_job = valid_job();
        future_job.schema_version = CURRENT_CONTRACT_VERSION + 1;
        let payload = serde_json::to_vec(&future_job).expect("job serializes");
        assert!(matches!(
            decode_job(Some(&payload)),
            Err(ContractError::FutureVersion { .. })
        ));

        let mut invalid_job = valid_job();
        invalid_job.slice.total = 0;
        let payload = serde_json::to_vec(&invalid_job).expect("job serializes");
        assert!(matches!(
            decode_job(Some(&payload)),
            Err(ContractError::InvalidSlice(_))
        ));

        let mut invalid_result = valid_result();
        invalid_result.attempt = MAX_CONTRACT_ATTEMPT + 1;
        let payload = serde_json::to_vec(&invalid_result).expect("result serializes");
        assert!(matches!(
            decode_result(Some(&payload)),
            Err(ContractError::InvalidResult(_))
        ));
    }

    #[test]
    fn producer_serialization_rejects_invalid_local_contracts() {
        assert!(serialize_validated_job(&valid_job()).is_ok());
        assert!(serialize_validated_result(&valid_result()).is_ok());

        let mut invalid_job = valid_job();
        invalid_job.plan_fingerprint.clear();
        assert!(serialize_validated_job(&invalid_job).is_err());

        let mut invalid_result = valid_result();
        invalid_result.total = 1;
        assert!(serialize_validated_result(&invalid_result).is_err());
    }

    #[test]
    fn dlq_serialization_validates_identity_and_bounds_reasons() {
        let mut failed = valid_failed_job();
        failed.reason = "failure-🔥".repeat(MAX_CONTRACT_ID_BYTES);
        let serialized = serialize_validated_failed_job(&failed).expect("valid failed job");
        let serialized_failed: FailedScenarioJob =
            serde_json::from_str(&serialized).expect("failed job decodes");
        assert!(serialized_failed.reason.len() <= MAX_CONTRACT_ID_BYTES);
        assert!(
            serialized_failed
                .reason
                .ends_with(POISON_REASON_TRUNCATION_MARKER)
        );

        failed.event_id = "caller-selected".to_string();
        assert!(serialize_validated_failed_job(&failed).is_err());

        let mut poison = poison_record(
            "pulse.jobs",
            0,
            4,
            None,
            Some(b"bad-json"),
            "malformed".to_string(),
        );
        assert!(serialize_validated_poison(&poison).is_ok());
        poison.payload_original_bytes = Some(1);
        assert!(serialize_validated_poison(&poison).is_err());
    }

    #[test]
    fn poison_record_preserves_binary_source_data_and_has_stable_identity() {
        let key = [0, 1, 2, 255];
        let payload = [255, 254, 0, 10];
        let first = poison_record(
            "pulse.jobs",
            3,
            42,
            Some(&key),
            Some(&payload),
            "malformed".to_string(),
        );
        let second = poison_record(
            "pulse.jobs",
            3,
            42,
            Some(&key),
            Some(&payload),
            "another reason".to_string(),
        );

        assert_eq!(first.event_id, "poison:pulse.jobs:3:42");
        assert_eq!(first.event_id, second.event_id);
        assert_eq!(first.source_key_original_bytes, Some(4));
        assert!(!first.source_key_truncated);
        assert_eq!(first.payload_original_bytes, Some(4));
        assert!(!first.payload_truncated);
        assert_eq!(
            BASE64_STANDARD
                .decode(first.source_key_base64.unwrap())
                .unwrap(),
            key
        );
        assert_eq!(
            BASE64_STANDARD
                .decode(first.payload_base64.unwrap())
                .unwrap(),
            payload
        );
    }

    #[test]
    fn poison_reason_is_bounded_at_a_utf8_boundary() {
        let reason = "failure-🔥".repeat(MAX_CONTRACT_ID_BYTES);
        let record = poison_record("pulse.jobs", 0, 1, None, None, reason);

        assert!(record.reason.len() <= MAX_CONTRACT_ID_BYTES);
        assert!(record.reason.ends_with(POISON_REASON_TRUNCATION_MARKER));
        assert!(
            record
                .reason
                .is_char_boundary(record.reason.len() - POISON_REASON_TRUNCATION_MARKER.len())
        );
    }

    #[test]
    fn default_maximum_poison_evidence_fits_the_producer_message_limit() {
        let consumer = KafkaConsumerConfig::default();
        let producer = KafkaProducerConfig::default();
        let payload = vec![0xff; consumer.partition_fetch_max_bytes - 1];
        let record = poison_record(
            "pulse.scenario.jobs",
            2,
            i64::MAX,
            Some(&[0xff]),
            Some(&payload),
            "x".repeat(MAX_CONTRACT_ID_BYTES * 2),
        );
        let serialized = serde_json::to_vec(&record).expect("poison record serializes");
        let kafka_record_bytes = serialized.len() + record.event_id.len();

        assert!(kafka_record_bytes <= producer.message_max_bytes);
        assert_eq!(
            record.payload_original_bytes,
            Some(u64::try_from(payload.len()).expect("test payload length fits u64"))
        );
        assert!(record.payload_truncated);
        assert_eq!(
            BASE64_STANDARD
                .decode(record.payload_base64.as_deref().expect("payload evidence"))
                .expect("valid base64")
                .len(),
            MAX_POISON_EVIDENCE_FIELD_BYTES
        );
    }
}
