use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rdkafka::ClientConfig;
use rdkafka::Message;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use rdkafka::types::RDKafkaErrorCode;

use crate::application::service::{
    CommitableJob, DlqPublisher, JobConsumer, JobPublisher, ResultPublisher,
};
use crate::domain::contracts::{FailedScenarioJob, ScenarioJob, ScenarioRunResult};

pub struct KafkaJobPublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaJobPublisher {
    pub fn new(
        brokers: &str,
        topic: impl Into<String>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .set(
                "queue.buffering.max.messages",
                queue_capacity.max(1).to_string(),
            )
            .create()
            .map_err(|e| format!("failed to create kafka producer: {e}"))?;

        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }
}

#[async_trait]
impl JobPublisher for KafkaJobPublisher {
    async fn publish_job(&self, key: &str, job: &ScenarioJob) -> Result<(), String> {
        let payload = serde_json::to_string(job)
            .map_err(|e| format!("failed to serialize scenario job: {e}"))?;

        self.producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| format!("failed to publish job: {e}"))?;

        Ok(())
    }
}

pub struct KafkaResultPublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaResultPublisher {
    pub fn new(
        brokers: &str,
        topic: impl Into<String>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .set(
                "queue.buffering.max.messages",
                queue_capacity.max(1).to_string(),
            )
            .create()
            .map_err(|e| format!("failed to create kafka producer: {e}"))?;

        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }
}

#[async_trait]
impl ResultPublisher for KafkaResultPublisher {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String> {
        let key = &result.execution_key;
        let payload = serde_json::to_string(result)
            .map_err(|e| format!("failed to serialize scenario result: {e}"))?;

        self.producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| format!("failed to publish result: {e}"))?;

        Ok(())
    }
}

pub struct KafkaJobConsumer {
    consumer: Arc<StreamConsumer>,
}

impl KafkaJobConsumer {
    pub fn new(
        brokers: &str,
        group_id: &str,
        topic: &str,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("group.id", group_id)
            .set("enable.auto.commit", "false")
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("auto.offset.reset", "earliest")
            .set(
                "queued.max.messages.kbytes",
                queue_capacity.max(1).to_string(),
            )
            .create()
            .map_err(|e| format!("failed to create kafka consumer: {e}"))?;

        consumer
            .subscribe(&[topic])
            .map_err(|e| format!("failed to subscribe topic: {e}"))?;

        Ok(Self {
            consumer: Arc::new(consumer),
        })
    }
}

pub struct KafkaDlqPublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaDlqPublisher {
    pub fn new(
        brokers: &str,
        topic: impl Into<String>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .set(
                "queue.buffering.max.messages",
                queue_capacity.max(1).to_string(),
            )
            .create()
            .map_err(|e| format!("failed to create kafka producer: {e}"))?;

        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }
}

#[async_trait]
impl DlqPublisher for KafkaDlqPublisher {
    async fn publish_failed_job(&self, key: &str, job: &FailedScenarioJob) -> Result<(), String> {
        let payload = serde_json::to_string(job)
            .map_err(|e| format!("failed to serialize failed scenario job: {e}"))?;

        self.producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| format!("failed to publish dead-letter job: {e}"))?;

        Ok(())
    }
}

pub struct ConsumedJob {
    topic: String,
    partition: i32,
    offset: i64,
    pub job: ScenarioJob,
    consumer: Arc<StreamConsumer>,
}

#[async_trait]
impl JobConsumer for KafkaJobConsumer {
    type Item = ConsumedJob;

    async fn recv(&self) -> Result<Option<Self::Item>, String> {
        let msg = self
            .consumer
            .recv()
            .await
            .map_err(|e| format!("kafka receive error: {e}"))?;

        let payload = msg
            .payload()
            .ok_or_else(|| "received message without payload".to_string())?;

        let job: ScenarioJob =
            serde_json::from_slice(payload).map_err(|e| format!("invalid job payload: {e}"))?;

        let topic = msg.topic().to_string();
        let partition = msg.partition();
        let offset = msg.offset();

        Ok(Some(ConsumedJob {
            topic,
            partition,
            offset,
            job,
            consumer: self.consumer.clone(),
        }))
    }
}

impl ConsumedJob {
    fn commit_inner(self) -> Result<(), String> {
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition_offset(&self.topic, self.partition, Offset::Offset(self.offset + 1))
            .map_err(|e| format!("failed to build commit offset: {e}"))?;
        self.consumer
            .commit(&tpl, CommitMode::Async)
            .map_err(|e| format!("failed to commit message: {e}"))?;
        Ok(())
    }
}

impl CommitableJob for ConsumedJob {
    fn job(&self) -> &ScenarioJob {
        &self.job
    }

    fn commit(self) -> Result<(), String> {
        self.commit_inner()
    }
}

pub async fn ensure_topics(brokers: &str, topics: &[(&str, i32, i32)]) -> Result<(), String> {
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .map_err(|e| format!("failed to create kafka admin client: {e}"))?;

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
        .map_err(|e| format!("failed to create kafka topics: {e}"))?;

    for result in results {
        match result {
            Ok(_) => {}
            Err((_, RDKafkaErrorCode::TopicAlreadyExists)) => {}
            Err((topic, code)) => {
                return Err(format!("failed to ensure kafka topic {topic}: {code:?}"));
            }
        }
    }

    Ok(())
}
