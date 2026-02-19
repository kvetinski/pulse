use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use redis::{AsyncCommands, Client, Script};
use tokio::sync::Mutex;

use crate::application::service::{DueStateStore, IdempotencyStore, LeaderElector};
use crate::domain::scenario::RepeatPolicy;

pub struct RedisLeaderElector {
    client: Client,
    lock_key: String,
    node_id: String,
    lock_ttl_ms: u64,
    is_leader: Mutex<bool>,
}

impl RedisLeaderElector {
    pub fn new(client: Client, lock_key: String, node_id: String, lock_ttl_ms: u64) -> Self {
        Self {
            client,
            lock_key,
            node_id,
            lock_ttl_ms,
            is_leader: Mutex::new(false),
        }
    }

    async fn acquire_lock(&self) -> bool {
        let mut conn = match self.client.get_multiplexed_tokio_connection().await {
            Ok(c) => c,
            Err(_) => return false,
        };

        let result: redis::RedisResult<Option<String>> = redis::cmd("SET")
            .arg(&self.lock_key)
            .arg(&self.node_id)
            .arg("NX")
            .arg("PX")
            .arg(self.lock_ttl_ms)
            .query_async(&mut conn)
            .await;

        matches!(result, Ok(Some(_)))
    }

    async fn renew_lock(&self) -> bool {
        let mut conn = match self.client.get_multiplexed_tokio_connection().await {
            Ok(c) => c,
            Err(_) => return false,
        };

        let script = Script::new(
            r#"
            if redis.call('GET', KEYS[1]) == ARGV[1] then
                return redis.call('PEXPIRE', KEYS[1], ARGV[2])
            end
            return 0
            "#,
        );

        let renewed: redis::RedisResult<i32> = script
            .key(&self.lock_key)
            .arg(&self.node_id)
            .arg(self.lock_ttl_ms)
            .invoke_async(&mut conn)
            .await;

        matches!(renewed, Ok(v) if v == 1)
    }
}

#[async_trait]
impl LeaderElector for RedisLeaderElector {
    async fn try_acquire_or_renew(&self) -> bool {
        let mut is_leader = self.is_leader.lock().await;
        let ok = if *is_leader {
            self.renew_lock().await
        } else {
            self.acquire_lock().await
        };

        *is_leader = ok;
        ok
    }

    async fn relinquish(&self) {
        let mut conn = match self.client.get_multiplexed_tokio_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };

        let script = Script::new(
            r#"
            if redis.call('GET', KEYS[1]) == ARGV[1] then
                return redis.call('DEL', KEYS[1])
            end
            return 0
            "#,
        );

        let _: redis::RedisResult<i32> = script
            .key(&self.lock_key)
            .arg(&self.node_id)
            .invoke_async(&mut conn)
            .await;

        let mut is_leader = self.is_leader.lock().await;
        *is_leader = false;
    }
}

pub struct RedisDueStateStore {
    client: Client,
    schedule_prefix: String,
}

impl RedisDueStateStore {
    pub fn new(client: Client, schedule_prefix: String) -> Self {
        Self {
            client,
            schedule_prefix,
        }
    }

    fn key_for(&self, scenario_id: &str) -> String {
        format!("{}:{}", self.schedule_prefix, scenario_id)
    }
}

#[async_trait]
impl DueStateStore for RedisDueStateStore {
    async fn claim_due(&self, scenario_id: &str, repeat: RepeatPolicy) -> bool {
        let mut conn = match self.client.get_multiplexed_tokio_connection().await {
            Ok(c) => c,
            Err(_) => return false,
        };

        let now_ms = now_unix_ms() as i64;
        let (is_once, repeat_ms) = match repeat {
            RepeatPolicy::Once => (1_i32, 0_i64),
            RepeatPolicy::Every(interval) => (0_i32, interval.as_millis() as i64),
        };

        let key = self.key_for(scenario_id);
        let script = Script::new(
            r#"
            local next_at = redis.call('HGET', KEYS[1], 'next_at')
            local once_done = redis.call('HGET', KEYS[1], 'once_done')

            if once_done == '1' then
                return 0
            end

            if not next_at then
                next_at = '0'
            end

            if tonumber(next_at) > tonumber(ARGV[1]) then
                return 0
            end

            if tonumber(ARGV[2]) == 1 then
                redis.call('HSET', KEYS[1], 'once_done', '1')
            else
                redis.call('HSET', KEYS[1], 'next_at', tostring(tonumber(ARGV[1]) + tonumber(ARGV[3])))
            end

            return 1
            "#,
        );

        let claimed: redis::RedisResult<i32> = script
            .key(key)
            .arg(now_ms)
            .arg(is_once)
            .arg(repeat_ms)
            .invoke_async(&mut conn)
            .await;

        matches!(claimed, Ok(v) if v == 1)
    }
}

pub struct RedisIdempotencyStore {
    client: Client,
    key_prefix: String,
    ttl: Duration,
}

impl RedisIdempotencyStore {
    pub fn new(client: Client, key_prefix: String, ttl: Duration) -> Self {
        Self {
            client,
            key_prefix,
            ttl,
        }
    }

    fn key_for(&self, execution_key: &str) -> String {
        format!("{}:{}", self.key_prefix, execution_key)
    }
}

#[async_trait]
impl IdempotencyStore for RedisIdempotencyStore {
    async fn claim_once(&self, execution_key: &str) -> bool {
        let mut conn = match self.client.get_multiplexed_tokio_connection().await {
            Ok(c) => c,
            Err(_) => return false,
        };

        let key = self.key_for(execution_key);
        let set: redis::RedisResult<bool> = conn.set_nx(&key, "1").await;
        if !matches!(set, Ok(true)) {
            return false;
        }

        let ttl_secs = self.ttl.as_secs().min(i64::MAX as u64) as i64;
        let _expire: redis::RedisResult<bool> = conn.expire(&key, ttl_secs).await;
        true
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time must be after epoch")
        .as_millis()
}

pub type SharedRedisLeaderElector = Arc<RedisLeaderElector>;
pub type SharedRedisDueStateStore = Arc<RedisDueStateStore>;
pub type SharedRedisIdempotencyStore = Arc<RedisIdempotencyStore>;
