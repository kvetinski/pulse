use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use redis::{Client, Script};

use super::redis_operation;
use crate::application::aggregation::{
    DurableRunSummary, RunAggregationStore, RunAggregationStoreError, RunAggregationUpdate,
    RunExpiryOutcome, RunFinalizationOutcome, SummaryAcknowledgement,
};
use crate::domain::contracts::{
    ErrorCount, LatencyBucket, MAX_CONTRACT_ERROR_KIND_BYTES, MAX_CONTRACT_EXACT_INTEGER,
    MAX_CONTRACT_HISTOGRAM_BUCKETS, MAX_CONTRACT_SLICES, ScenarioRunResult, ScenarioRunSummary,
    ScenarioRunSummaryStatus,
};
use crate::domain::coordination::CoordinationError;
use crate::domain::error::ContractError;

const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_MAX_ERROR_KINDS: usize = 64;
const DEFAULT_MAX_ACTIVE_RUNS: usize = 10_000;
const DEFAULT_MAX_SCAN_LIMIT: usize = 1_024;
const MAX_IDENTITY_BYTES: usize = 1_024;
const MAX_ERROR_KIND_BYTES: usize = MAX_CONTRACT_ERROR_KIND_BYTES;
const MAX_SLICES_PER_RUN: u32 = MAX_CONTRACT_SLICES;
const MAX_HISTOGRAM_BUCKETS: usize = MAX_CONTRACT_HISTOGRAM_BUCKETS;
// Redis executes Lua 5.1 with IEEE-754 doubles. Keeping counters at or below
// 2^53-1 makes every validation and increment comparison exact.
const MAX_EXACT_LUA_INTEGER: u64 = MAX_CONTRACT_EXACT_INTEGER;

const INGEST_SCRIPT: &str = r#"
local function response(code, message)
    return {code, message, redis.call('HGETALL', KEYS[1])}
end

local max_exact = 9007199254740991
local scenario_id = ARGV[1]
local run_id = ARGV[2]
local schema_version = ARGV[3]
local expected_slices = tonumber(ARGV[4])
local slice_index = tonumber(ARGV[5])
local execution_key = ARGV[6]
local histogram_bounds = ARGV[7]
local incoming_total = tonumber(ARGV[8])
local incoming_success = tonumber(ARGV[9])
local incoming_failure = tonumber(ARGV[10])
local retention_ms = tonumber(ARGV[11])
local max_error_kinds = tonumber(ARGV[12])
local max_active_runs = tonumber(ARGV[13])
local partial_timeout_ms = tonumber(ARGV[14])
local bucket_count = tonumber(ARGV[15])
local now_parts = redis.call('TIME')
local redis_now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
local received_at = redis_now_ms

if not expected_slices or expected_slices < 1
    or not slice_index or slice_index < 0 or slice_index >= expected_slices
    or not incoming_total or incoming_total < 0 or incoming_total > max_exact
    or not incoming_success or incoming_success < 0 or incoming_success > max_exact
    or not incoming_failure or incoming_failure < 0 or incoming_failure > max_exact
    or incoming_success + incoming_failure ~= incoming_total
    or not retention_ms or retention_ms < 1
    or not max_error_kinds or max_error_kinds < 1
    or not max_active_runs or max_active_runs < 1
    or not partial_timeout_ms or partial_timeout_ms < 1
    or received_at > max_exact - partial_timeout_ms
    or not bucket_count or bucket_count < 0 then
    return {5, 'invalid aggregation script arguments', {}}
end

local bucket_start = 16
local error_count_index = bucket_start + bucket_count
local error_pair_count = tonumber(ARGV[error_count_index])
if not error_pair_count or error_pair_count < 0 then
    return {5, 'invalid aggregation error-count arguments', {}}
end

for index = 0, bucket_count - 1 do
    local count = tonumber(ARGV[bucket_start + index])
    if not count or count < 0 or count > max_exact then
        return {5, 'latency bucket count is outside the exact Redis Lua range', {}}
    end
end
for index = 0, error_pair_count - 1 do
    local count = tonumber(ARGV[error_count_index + 2 + index * 2])
    if not count or count < 0 or count > max_exact then
        return {5, 'error count is outside the exact Redis Lua range', {}}
    end
end

local exists = redis.call('EXISTS', KEYS[1]) == 1
local status = 'open'
local function refresh_retention()
    local pending = tonumber(redis.call('HGET', KEYS[1], 'summary_pending_revision') or '0')
    local acknowledged = tonumber(redis.call('HGET', KEYS[1], 'summary_ack_revision') or '0')
    if pending > acknowledged then
        -- A Kafka-acknowledged summary is the terminal durable outcome. Never
        -- expire its Redis outbox source before that acknowledgement exists.
        redis.call('PERSIST', KEYS[1])
        redis.call('ZADD', KEYS[4], max_exact, run_id)
    else
        redis.call('PEXPIRE', KEYS[1], retention_ms)
        redis.call('ZADD', KEYS[4], redis_now_ms + retention_ms, run_id)
    end
end
redis.call('ZREMRANGEBYSCORE', KEYS[4], '-inf', redis_now_ms)
if not exists and redis.call('ZCARD', KEYS[4]) >= max_active_runs then
    return {8, 'bounded active-run capacity exceeded', {}}
end
if exists then
    local stored_histogram_bounds = redis.call('HGET', KEYS[1], 'histogram_bounds')
    if redis.call('HGET', KEYS[1], 'scenario_id') ~= scenario_id
        or redis.call('HGET', KEYS[1], 'run_id') ~= run_id
        or redis.call('HGET', KEYS[1], 'schema_version') ~= schema_version
        or tonumber(redis.call('HGET', KEYS[1], 'expected_slices') or '-1') ~= expected_slices
        or (stored_histogram_bounds ~= '' and stored_histogram_bounds ~= histogram_bounds) then
        return response(5, 'scenario, schema, expected slices, or histogram bounds changed')
    end
    if stored_histogram_bounds == '' then
        if tonumber(redis.call('HGET', KEYS[1], 'received_slices') or '-1') ~= 0 then
            return response(5, 'registered run histogram is unset after accepting slices')
        end
        redis.call('HSET', KEYS[1], 'histogram_bounds', histogram_bounds)
    end
    status = redis.call('HGET', KEYS[1], 'status') or ''
    if status ~= 'open' and status ~= 'partial' and status ~= 'timed_out'
        and status ~= 'cancelled' and status ~= 'complete' then
        return response(5, 'stored aggregation status is invalid')
    end

    local existing_slice = redis.call('HGET', KEYS[1], 'slice:' .. tostring(slice_index))
    if existing_slice then
        if existing_slice == execution_key then
            refresh_retention()
            return response(2, '')
        end
        return response(5, 'slice index already belongs to a different execution identity')
    end
    local existing_execution = redis.call('HGET', KEYS[1], 'execution:' .. execution_key)
    if existing_execution then
        if tonumber(existing_execution) == slice_index then
            refresh_retention()
            return response(2, '')
        end
        return response(5, 'execution identity already belongs to a different slice')
    end
    if status == 'complete' or status == 'cancelled' then
        return response(5, 'terminal run cannot accept an unrecorded slice')
    end
end

local current_received = 0
local current_error_kinds = 0
if exists then
    current_received = tonumber(redis.call('HGET', KEYS[1], 'received_slices') or '-1')
    current_error_kinds = tonumber(redis.call('HGET', KEYS[1], 'error_kind_count') or '-1')
    if not current_received or current_received < 0 or current_received >= expected_slices
        or not current_error_kinds or current_error_kinds < 0 then
        return response(5, 'stored aggregation counters are invalid')
    end
end

local additional_error_kinds = 0
for index = 0, error_pair_count - 1 do
    local kind = ARGV[error_count_index + 1 + index * 2]
    if redis.call('HEXISTS', KEYS[1], 'error:' .. kind) == 0 then
        additional_error_kinds = additional_error_kinds + 1
    end
end
if current_error_kinds + additional_error_kinds > max_error_kinds then
    return response(6, 'bounded error-kind capacity exceeded')
end

local function sum_is_exact(field, incoming)
    local current = tonumber(redis.call('HGET', KEYS[1], field) or '0')
    return current and current >= 0 and current <= max_exact
        and incoming <= max_exact - current
end

if not sum_is_exact('total', incoming_total)
    or not sum_is_exact('success', incoming_success)
    or not sum_is_exact('failure', incoming_failure) then
    return response(9, 'aggregate result counter exceeds the exact Redis Lua range')
end
for index = 0, bucket_count - 1 do
    if not sum_is_exact('latency:' .. tostring(index), tonumber(ARGV[bucket_start + index])) then
        return response(9, 'aggregate latency counter exceeds the exact Redis Lua range')
    end
end
for index = 0, error_pair_count - 1 do
    local kind = ARGV[error_count_index + 1 + index * 2]
    local count = tonumber(ARGV[error_count_index + 2 + index * 2])
    if not sum_is_exact('error:' .. kind, count) then
        return response(9, 'aggregate error counter exceeds the exact Redis Lua range')
    end
end

if not exists then
    redis.call('HSET', KEYS[1],
        'scenario_id', scenario_id,
        'run_id', run_id,
        'schema_version', schema_version,
        'expected_slices', tostring(expected_slices),
        'histogram_bounds', histogram_bounds,
        'status', 'open',
        'received_slices', '0',
        'total', '0',
        'success', '0',
        'failure', '0',
        'error_kind_count', '0',
        'first_result_at', tostring(received_at),
        'last_result_at', tostring(received_at),
        'deadline_at', tostring(received_at + partial_timeout_ms),
        'finalized_at', '0',
        'summary_revision', '0',
        'summary_pending_revision', '0',
        'summary_ack_revision', '0')
    redis.call('ZADD', KEYS[2], received_at + partial_timeout_ms, run_id)
    status = 'open'
end

redis.call('HSET', KEYS[1],
    'slice:' .. tostring(slice_index), execution_key,
    'execution:' .. execution_key, tostring(slice_index))
local received_slices = redis.call('HINCRBY', KEYS[1], 'received_slices', 1)
redis.call('HINCRBY', KEYS[1], 'total', ARGV[8])
redis.call('HINCRBY', KEYS[1], 'success', ARGV[9])
redis.call('HINCRBY', KEYS[1], 'failure', ARGV[10])
for index = 0, bucket_count - 1 do
    redis.call('HINCRBY', KEYS[1], 'latency:' .. tostring(index), ARGV[bucket_start + index])
end
for index = 0, error_pair_count - 1 do
    local kind = ARGV[error_count_index + 1 + index * 2]
    local count = ARGV[error_count_index + 2 + index * 2]
    local field = 'error:' .. kind
    if redis.call('HEXISTS', KEYS[1], field) == 0 then
        redis.call('HINCRBY', KEYS[1], 'error_kind_count', 1)
    end
    redis.call('HINCRBY', KEYS[1], field, count)
end
local previous_last = tonumber(redis.call('HGET', KEYS[1], 'last_result_at') or '0')
local first_result = tonumber(redis.call('HGET', KEYS[1], 'first_result_at') or '0')
if first_result == 0 then
    redis.call('HSET', KEYS[1], 'first_result_at', tostring(received_at))
end
if received_at > previous_last then
    redis.call('HSET', KEYS[1], 'last_result_at', tostring(received_at))
end
refresh_retention()

if received_slices == expected_slices then
    local revision = redis.call('HINCRBY', KEYS[1], 'summary_revision', 1)
    redis.call('HSET', KEYS[1],
        'status', 'complete',
        'finalized_at', tostring(received_at),
        'summary_pending_revision', tostring(revision))
    redis.call('ZREM', KEYS[2], run_id)
    redis.call('ZADD', KEYS[3], received_at, run_id)
    refresh_retention()
    if status == 'partial' or status == 'timed_out' then
        return response(4, '')
    end
    return response(3, '')
end
if status == 'partial' or status == 'timed_out' then
    return response(7, '')
end
return response(1, '')
"#;

const MARK_EXPIRED_SCRIPT: &str = r#"
local function response(code, message)
    return {code, message, redis.call('HGETALL', KEYS[1])}
end

local retention_ms = tonumber(ARGV[1])
local run_id = ARGV[2]
if redis.call('EXISTS', KEYS[1]) == 0 then
    redis.call('ZREM', KEYS[2], run_id)
    redis.call('ZREM', KEYS[3], run_id)
    redis.call('ZREM', KEYS[4], run_id)
    return {0, '', {}}
end
local max_exact = 9007199254740991
local now_parts = redis.call('TIME')
local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
if not retention_ms or retention_ms < 1 or now_ms > max_exact - retention_ms then
    return response(4, 'invalid expiration arguments')
end

local status = redis.call('HGET', KEYS[1], 'status') or ''
if status == 'partial' or status == 'timed_out'
    or status == 'cancelled' or status == 'complete' then
    redis.call('ZREM', KEYS[2], run_id)
    local pending = tonumber(redis.call('HGET', KEYS[1], 'summary_pending_revision') or '0')
    local acknowledged = tonumber(redis.call('HGET', KEYS[1], 'summary_ack_revision') or '0')
    if pending > acknowledged then
        redis.call('PERSIST', KEYS[1])
        redis.call('ZADD', KEYS[4], max_exact, run_id)
    end
    return response(3, '')
end
if status ~= 'open' then
    return response(4, 'stored aggregation status is invalid')
end
local deadline_at = tonumber(redis.call('HGET', KEYS[1], 'deadline_at') or '-1')
if not deadline_at or deadline_at < 0 then
    return response(4, 'stored aggregation deadline is invalid')
end
if now_ms < deadline_at then
    return response(1, tostring(deadline_at - now_ms))
end

local revision = redis.call('HINCRBY', KEYS[1], 'summary_revision', 1)
redis.call('HSET', KEYS[1],
    'status', 'timed_out',
    'finalized_at', tostring(now_ms),
    'summary_pending_revision', tostring(revision))
redis.call('ZREM', KEYS[2], run_id)
redis.call('ZADD', KEYS[3], now_ms, run_id)
redis.call('PERSIST', KEYS[1])
redis.call('ZADD', KEYS[4], max_exact, run_id)
return response(2, '')
"#;

const FINALIZE_RUN_SCRIPT: &str = r#"
local function response(code, message)
    return {code, message, redis.call('HGETALL', KEYS[1])}
end
local requested_status = ARGV[1]
local retention_ms = tonumber(ARGV[2])
local run_id = ARGV[3]
local max_exact = 9007199254740991
local now_parts = redis.call('TIME')
local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
if requested_status ~= 'partial' and requested_status ~= 'cancelled' then
    return {3, 'only partial or cancelled explicit finalization is supported', {}}
end
if not retention_ms or retention_ms < 1 or now_ms > max_exact - retention_ms then
    return {3, 'invalid explicit finalization arguments', {}}
end
if redis.call('EXISTS', KEYS[1]) == 0 then
    redis.call('ZREM', KEYS[2], run_id)
    redis.call('ZREM', KEYS[3], run_id)
    redis.call('ZREM', KEYS[4], run_id)
    return {0, '', {}}
end
local status = redis.call('HGET', KEYS[1], 'status') or ''
if status ~= 'open' then
    if status == 'partial' or status == 'timed_out'
        or status == 'cancelled' or status == 'complete' then
        redis.call('ZREM', KEYS[2], run_id)
        local pending = tonumber(redis.call('HGET', KEYS[1], 'summary_pending_revision') or '0')
        local acknowledged = tonumber(redis.call('HGET', KEYS[1], 'summary_ack_revision') or '0')
        if pending > acknowledged then
            redis.call('PERSIST', KEYS[1])
            redis.call('ZADD', KEYS[4], max_exact, run_id)
        end
        return response(2, '')
    end
    return response(3, 'stored aggregation status is invalid')
end

local revision = redis.call('HINCRBY', KEYS[1], 'summary_revision', 1)
redis.call('HSET', KEYS[1],
    'status', requested_status,
    'finalized_at', tostring(now_ms),
    'summary_pending_revision', tostring(revision))
redis.call('ZREM', KEYS[2], run_id)
redis.call('ZADD', KEYS[3], now_ms, run_id)
redis.call('PERSIST', KEYS[1])
redis.call('ZADD', KEYS[4], max_exact, run_id)
return response(1, '')
"#;

const DUE_RUNS_SCRIPT: &str = r#"
local limit = tonumber(ARGV[1])
if not limit or limit < 1 then
    return redis.error_reply('aggregation scan limit must be positive')
end
local now_parts = redis.call('TIME')
local redis_now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
return redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', redis_now_ms, 'LIMIT', 0, limit)
"#;

const ACKNOWLEDGE_SUMMARY_SCRIPT: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then
    redis.call('ZREM', KEYS[2], ARGV[2])
    redis.call('ZREM', KEYS[3], ARGV[2])
    return {0, 0, ''}
end
local revision = tonumber(ARGV[1])
local run_id = ARGV[2]
local retention_ms = tonumber(ARGV[3])
local current = tonumber(redis.call('HGET', KEYS[1], 'summary_revision') or '0')
local acknowledged = tonumber(redis.call('HGET', KEYS[1], 'summary_ack_revision') or '0')
local pending = tonumber(redis.call('HGET', KEYS[1], 'summary_pending_revision') or '0')
if not revision or revision < 1 or not retention_ms or retention_ms < 1
    or not current or current < 1
    or not acknowledged or acknowledged < 0 or not pending or pending < 0 then
    return {4, current or 0, 'stored summary revision is invalid'}
end
if revision > current then
    return {4, current, 'cannot acknowledge a future summary revision'}
end
if revision < current then
    if revision > acknowledged then
        redis.call('HSET', KEYS[1], 'summary_ack_revision', tostring(revision))
    end
    return {3, current, ''}
end
if acknowledged >= revision then
    if pending <= acknowledged then
        redis.call('ZREM', KEYS[2], run_id)
        redis.call('PEXPIRE', KEYS[1], retention_ms)
        local now_parts = redis.call('TIME')
        local redis_now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
        redis.call('ZADD', KEYS[3], redis_now_ms + retention_ms, run_id)
    end
    return {2, current, ''}
end
redis.call('HSET', KEYS[1], 'summary_ack_revision', tostring(revision))
if pending == revision then
    redis.call('ZREM', KEYS[2], run_id)
    redis.call('PEXPIRE', KEYS[1], retention_ms)
    local now_parts = redis.call('TIME')
    local redis_now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
    redis.call('ZADD', KEYS[3], redis_now_ms + retention_ms, run_id)
end
return {1, current, ''}
"#;

const CLEAN_OUTBOX_SCRIPT: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then
    redis.call('ZREM', KEYS[2], ARGV[1])
    return 1
end
local pending = tonumber(redis.call('HGET', KEYS[1], 'summary_pending_revision') or '0')
local acknowledged = tonumber(redis.call('HGET', KEYS[1], 'summary_ack_revision') or '0')
if pending <= acknowledged then
    redis.call('ZREM', KEYS[2], ARGV[1])
    return 1
end
return 0
"#;

/// Atomic, duplicate-tolerant run aggregation with durable deadline and
/// summary-publication indexes. All keys share a Redis Cluster hash slot.
/// Redis `TIME` is authoritative for first-result, deadline, finalization, and
/// retention timestamps; caller-provided clock observations are ignored.
pub struct RedisRunAggregationStore {
    client: Client,
    key_prefix: String,
    partial_timeout: Duration,
    retention_ttl: Duration,
    operation_timeout: Duration,
    max_error_kinds: usize,
    max_active_runs: usize,
    max_scan_limit: usize,
}

#[derive(Debug)]
struct NormalizedResult {
    histogram_bounds: String,
    histogram_counts: Vec<u64>,
    error_counts: Vec<(String, u64)>,
}

impl RedisRunAggregationStore {
    /// Creates a store whose deadline begins with the first received slice.
    /// Retention must exceed the deadline so incomplete runs remain discoverable.
    pub fn new(
        client: Client,
        key_prefix: String,
        partial_timeout: Duration,
        retention_ttl: Duration,
    ) -> Result<Self, RunAggregationStoreError> {
        if key_prefix.trim().is_empty()
            || key_prefix.len() > MAX_IDENTITY_BYTES
            || key_prefix.contains(['{', '}'])
        {
            return Err(invalid_state(
                "aggregation_config",
                "Redis aggregation key prefix must be non-empty, bounded, and contain no hash-tag braces",
            ));
        }
        checked_lua_duration_ms("aggregation_config", partial_timeout)?;
        checked_lua_duration_ms("aggregation_config", retention_ttl)?;
        if retention_ttl <= partial_timeout {
            return Err(invalid_state(
                "aggregation_config",
                "aggregation retention TTL must exceed the partial-run timeout",
            ));
        }
        Ok(Self {
            client,
            key_prefix,
            partial_timeout,
            retention_ttl,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            max_error_kinds: DEFAULT_MAX_ERROR_KINDS,
            max_active_runs: DEFAULT_MAX_ACTIVE_RUNS,
            max_scan_limit: DEFAULT_MAX_SCAN_LIMIT,
        })
    }

    pub fn with_operation_timeout(
        mut self,
        operation_timeout: Duration,
    ) -> Result<Self, RunAggregationStoreError> {
        checked_duration_ms("aggregation_config", operation_timeout)?;
        self.operation_timeout = operation_timeout;
        Ok(self)
    }

    pub fn with_max_error_kinds(
        mut self,
        max_error_kinds: usize,
    ) -> Result<Self, RunAggregationStoreError> {
        if max_error_kinds == 0
            || u64::try_from(max_error_kinds).map_or(true, |value| value > MAX_EXACT_LUA_INTEGER)
        {
            return Err(invalid_state(
                "aggregation_config",
                "maximum error kinds must fit in Redis and be greater than zero",
            ));
        }
        self.max_error_kinds = max_error_kinds;
        Ok(self)
    }

    pub fn with_max_active_runs(
        mut self,
        max_active_runs: usize,
    ) -> Result<Self, RunAggregationStoreError> {
        if max_active_runs == 0
            || u64::try_from(max_active_runs).map_or(true, |value| value > MAX_EXACT_LUA_INTEGER)
        {
            return Err(invalid_state(
                "aggregation_config",
                "maximum active runs must fit in Redis and be greater than zero",
            ));
        }
        self.max_active_runs = max_active_runs;
        Ok(self)
    }

    pub fn with_max_scan_limit(
        mut self,
        max_scan_limit: usize,
    ) -> Result<Self, RunAggregationStoreError> {
        if max_scan_limit == 0
            || u64::try_from(max_scan_limit).map_or(true, |value| value > MAX_EXACT_LUA_INTEGER)
        {
            return Err(invalid_state(
                "aggregation_config",
                "maximum aggregation scan limit must fit in Redis and be greater than zero",
            ));
        }
        self.max_scan_limit = max_scan_limit;
        Ok(self)
    }

    fn key_for_run(&self, run_id: &str) -> String {
        format!("{}:{{runs}}:run:{}", self.key_prefix, run_id)
    }

    fn deadline_key(&self) -> String {
        format!("{}:{{runs}}:deadlines", self.key_prefix)
    }

    fn outbox_key(&self) -> String {
        format!("{}:{{runs}}:summary-outbox", self.key_prefix)
    }

    fn active_key(&self) -> String {
        format!("{}:{{runs}}:active", self.key_prefix)
    }

    fn validate_scan_limit(&self, limit: usize) -> Result<i64, RunAggregationStoreError> {
        if limit == 0 || limit > self.max_scan_limit {
            return Err(invalid_state(
                "aggregation_scan",
                format!("scan limit must be within 1..={}", self.max_scan_limit),
            ));
        }
        i64::try_from(limit)
            .map_err(|_| invalid_state("aggregation_scan", "scan limit exceeds Redis range"))
    }

    async fn load_snapshot(
        &self,
        run_id: &str,
        operation: &'static str,
    ) -> Result<Vec<String>, RunAggregationStoreError> {
        let mut connection = run_redis_operation(
            operation,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        run_redis_operation(
            operation,
            self.operation_timeout,
            redis::cmd("HGETALL")
                .arg(self.key_for_run(run_id))
                .query_async::<Vec<String>>(&mut connection),
        )
        .await
    }

    fn normalize_result(
        &self,
        result: &ScenarioRunResult,
    ) -> Result<NormalizedResult, RunAggregationStoreError> {
        result.validate()?;
        if result.slice.total > MAX_SLICES_PER_RUN {
            return Err(invalid_state(
                "aggregation_ingest",
                format!("expected slices exceed the bounded maximum of {MAX_SLICES_PER_RUN}"),
            ));
        }
        if result.latency_histogram.len() > MAX_HISTOGRAM_BUCKETS {
            return Err(invalid_state(
                "aggregation_ingest",
                format!(
                    "latency histogram exceeds the bounded maximum of {MAX_HISTOGRAM_BUCKETS} buckets"
                ),
            ));
        }
        if result.error_breakdown.len() > self.max_error_kinds {
            return Err(RunAggregationStoreError::ErrorKindCapacity {
                max_error_kinds: self.max_error_kinds,
            });
        }
        validate_identity("scenario_id", &result.scenario_id)?;
        validate_identity("run_id", &result.run_id)?;
        validate_identity("execution_key", &result.execution_key)?;
        exact_result_integer("total", result.total)?;
        exact_result_integer("success", result.success)?;
        exact_result_integer("failure", result.failure)?;

        let histogram_bounds = result
            .latency_histogram
            .iter()
            .map(|bucket| bucket.upper_bound_ms.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut histogram_counts = Vec::with_capacity(result.latency_histogram.len());
        for bucket in &result.latency_histogram {
            exact_result_integer("latency bucket count", bucket.count)?;
            histogram_counts.push(bucket.count);
        }

        let mut errors = BTreeMap::<String, u64>::new();
        for error in &result.error_breakdown {
            if error.kind.trim().is_empty() || error.kind.len() > MAX_ERROR_KIND_BYTES {
                return Err(invalid_state(
                    "aggregation_ingest",
                    "error kind must be non-empty and at most 256 bytes",
                ));
            }
            exact_result_integer("error count", error.count)?;
            let count = errors.entry(error.kind.clone()).or_default();
            *count = count
                .checked_add(error.count)
                .ok_or_else(|| invalid_state("aggregation_ingest", "error count overflowed u64"))?;
            exact_result_integer("merged error count", *count)?;
        }
        if errors.len() > self.max_error_kinds {
            return Err(RunAggregationStoreError::ErrorKindCapacity {
                max_error_kinds: self.max_error_kinds,
            });
        }

        Ok(NormalizedResult {
            histogram_bounds,
            histogram_counts,
            error_counts: errors.into_iter().collect(),
        })
    }
}

#[async_trait]
impl RunAggregationStore for RedisRunAggregationStore {
    async fn ingest(
        &self,
        result: &ScenarioRunResult,
        _received_at_unix_ms: u128,
    ) -> Result<RunAggregationUpdate, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_ingest";
        let normalized = self.normalize_result(result)?;
        let retention_ms = checked_duration_ms(OPERATION, self.retention_ttl)?;
        let partial_timeout_ms = checked_duration_ms(OPERATION, self.partial_timeout)?;
        let key = self.key_for_run(&result.run_id);
        let deadline_key = self.deadline_key();
        let outbox_key = self.outbox_key();
        let active_key = self.active_key();
        let mut connection = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(INGEST_SCRIPT);
        let mut invocation = script.key(key);
        invocation.key(deadline_key).key(outbox_key).key(active_key);
        invocation
            .arg(&result.scenario_id)
            .arg(&result.run_id)
            .arg(result.schema_version)
            .arg(result.slice.total)
            .arg(result.slice.index)
            .arg(&result.execution_key)
            .arg(&normalized.histogram_bounds)
            .arg(result.total)
            .arg(result.success)
            .arg(result.failure)
            .arg(retention_ms)
            .arg(self.max_error_kinds)
            .arg(self.max_active_runs)
            .arg(partial_timeout_ms)
            .arg(normalized.histogram_counts.len());
        for count in &normalized.histogram_counts {
            invocation.arg(*count);
        }
        invocation.arg(normalized.error_counts.len());
        for (kind, count) in &normalized.error_counts {
            invocation.arg(kind).arg(*count);
        }
        let (code, message, snapshot): (i64, String, Vec<String>) = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            invocation.invoke_async(&mut connection),
        )
        .await?;

        match code {
            1 => {
                let (received_slices, expected_slices, _) = progress(&snapshot, OPERATION)?;
                Ok(RunAggregationUpdate::Accepted {
                    received_slices,
                    expected_slices,
                })
            }
            2 => {
                let (received_slices, expected_slices, status) = progress(&snapshot, OPERATION)?;
                Ok(RunAggregationUpdate::Duplicate {
                    received_slices,
                    expected_slices,
                    finalized_status: finalized_status(status, OPERATION)?,
                })
            }
            3 => Ok(RunAggregationUpdate::Completed(summary_from_snapshot(
                &snapshot, OPERATION,
            )?)),
            4 => Ok(RunAggregationUpdate::LateCompleted(summary_from_snapshot(
                &snapshot, OPERATION,
            )?)),
            5 if is_permanent_result_conflict(&message) => {
                Err(RunAggregationStoreError::InconsistentResult { message })
            }
            5 => Err(invalid_state(OPERATION, message)),
            6 => Err(RunAggregationStoreError::ErrorKindCapacity {
                max_error_kinds: self.max_error_kinds,
            }),
            7 => {
                let (received_slices, expected_slices, _) = progress(&snapshot, OPERATION)?;
                Ok(RunAggregationUpdate::LateAccepted {
                    received_slices,
                    expected_slices,
                })
            }
            8 => Err(RunAggregationStoreError::ActiveRunCapacity {
                max_active_runs: self.max_active_runs,
            }),
            9 => Err(RunAggregationStoreError::Contract(
                ContractError::InvalidResult(message),
            )),
            other => Err(invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn due_runs(
        &self,
        _now_unix_ms: u128,
        limit: usize,
    ) -> Result<Vec<String>, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_due_runs";
        let limit = self.validate_scan_limit(limit)?;
        let mut connection = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(DUE_RUNS_SCRIPT);
        run_redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(self.deadline_key())
                .arg(limit)
                .invoke_async::<Vec<String>>(&mut connection),
        )
        .await
    }

    async fn mark_expired(
        &self,
        run_id: &str,
        _now_unix_ms: u128,
    ) -> Result<RunExpiryOutcome, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_mark_expired";
        validate_identity("run_id", run_id)?;
        let retention_ms = checked_duration_ms(OPERATION, self.retention_ttl)?;
        let key = self.key_for_run(run_id);
        let mut connection = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(MARK_EXPIRED_SCRIPT);
        let (code, message, snapshot): (i64, String, Vec<String>) = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(key)
                .key(self.deadline_key())
                .key(self.outbox_key())
                .key(self.active_key())
                .arg(retention_ms)
                .arg(run_id)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            0 => Ok(RunExpiryOutcome::Missing),
            1 => {
                let retry_ms = message.parse::<u64>().map_err(|_| {
                    invalid_state(OPERATION, "Redis returned an invalid expiration delay")
                })?;
                Ok(RunExpiryOutcome::NotExpired {
                    retry_after: Duration::from_millis(retry_ms),
                })
            }
            2 => Ok(RunExpiryOutcome::MarkedTimedOut(summary_from_snapshot(
                &snapshot, OPERATION,
            )?)),
            3 => Ok(RunExpiryOutcome::AlreadyFinalized(summary_from_snapshot(
                &snapshot, OPERATION,
            )?)),
            4 => Err(invalid_state(OPERATION, message)),
            other => Err(invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn finalize_run(
        &self,
        run_id: &str,
        status: ScenarioRunSummaryStatus,
        _now_unix_ms: u128,
    ) -> Result<RunFinalizationOutcome, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_finalize_run";
        validate_identity("run_id", run_id)?;
        let status = match status {
            ScenarioRunSummaryStatus::Partial => "partial",
            ScenarioRunSummaryStatus::Cancelled => "cancelled",
            ScenarioRunSummaryStatus::Complete | ScenarioRunSummaryStatus::TimedOut => {
                return Err(invalid_state(
                    OPERATION,
                    "explicit finalization only accepts Partial or Cancelled",
                ));
            }
        };
        let retention_ms = checked_duration_ms(OPERATION, self.retention_ttl)?;
        let mut connection = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(FINALIZE_RUN_SCRIPT);
        let (code, message, snapshot): (i64, String, Vec<String>) = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(self.key_for_run(run_id))
                .key(self.deadline_key())
                .key(self.outbox_key())
                .key(self.active_key())
                .arg(status)
                .arg(retention_ms)
                .arg(run_id)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            0 => Ok(RunFinalizationOutcome::Missing),
            1 => Ok(RunFinalizationOutcome::Finalized(summary_from_snapshot(
                &snapshot, OPERATION,
            )?)),
            2 => Ok(RunFinalizationOutcome::AlreadyFinalized(
                summary_from_snapshot(&snapshot, OPERATION)?,
            )),
            3 => Err(invalid_state(OPERATION, message)),
            other => Err(invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn load_summary(
        &self,
        run_id: &str,
    ) -> Result<Option<DurableRunSummary>, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_load_summary";
        validate_identity("run_id", run_id)?;
        let snapshot = self.load_snapshot(run_id, OPERATION).await?;
        durable_summary_from_snapshot(&snapshot, OPERATION)
    }

    async fn pending_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<DurableRunSummary>, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_pending_summaries";
        let limit = self.validate_scan_limit(limit)?;
        let mut connection = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let run_ids = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            redis::cmd("ZRANGE")
                .arg(self.outbox_key())
                .arg(0)
                .arg(limit - 1)
                .query_async::<Vec<String>>(&mut connection),
        )
        .await?;
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipeline = redis::pipe();
        for run_id in &run_ids {
            pipeline.cmd("HGETALL").arg(self.key_for_run(run_id));
        }
        let snapshots: Vec<Vec<String>> = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            pipeline.query_async(&mut connection),
        )
        .await?;
        drop(connection);

        if snapshots.len() != run_ids.len() {
            return Err(invalid_state(
                OPERATION,
                "Redis aggregation snapshot batch length changed",
            ));
        }
        let mut summaries = Vec::with_capacity(run_ids.len());
        let mut stale = Vec::new();
        for (run_id, snapshot) in run_ids.into_iter().zip(snapshots) {
            match durable_summary_from_snapshot(&snapshot, OPERATION)? {
                Some(summary) if summary.pending_publication => summaries.push(summary),
                _ => stale.push(run_id),
            }
        }
        let mut cleanup = tokio::task::JoinSet::new();
        for run_id in stale {
            let client = self.client.clone();
            let run_key = self.key_for_run(&run_id);
            let outbox_key = self.outbox_key();
            let operation_timeout = self.operation_timeout;
            cleanup.spawn(async move {
                clean_outbox_if_settled(client, run_key, outbox_key, run_id, operation_timeout)
                    .await
            });
        }
        while let Some(result) = cleanup.join_next().await {
            result.map_err(|error| {
                invalid_state(OPERATION, format!("outbox cleanup task failed: {error}"))
            })??;
        }
        Ok(summaries)
    }

    async fn acknowledge_summary(
        &self,
        run_id: &str,
        revision: u64,
    ) -> Result<SummaryAcknowledgement, RunAggregationStoreError> {
        const OPERATION: &str = "aggregation_acknowledge_summary";
        validate_identity("run_id", run_id)?;
        if revision == 0 || revision > i64::MAX as u64 {
            return Err(invalid_state(
                OPERATION,
                "summary revision must be within 1..=i64::MAX",
            ));
        }
        let retention_ms = checked_duration_ms(OPERATION, self.retention_ttl)?;
        let mut connection = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(ACKNOWLEDGE_SUMMARY_SCRIPT);
        let (code, current, message): (i64, i64, String) = run_redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(self.key_for_run(run_id))
                .key(self.outbox_key())
                .key(self.active_key())
                .arg(revision)
                .arg(run_id)
                .arg(retention_ms)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            0 => Ok(SummaryAcknowledgement::Missing),
            1 => Ok(SummaryAcknowledgement::Acknowledged),
            2 => Ok(SummaryAcknowledgement::AlreadyAcknowledged),
            3 => Ok(SummaryAcknowledgement::Stale {
                current_revision: u64::try_from(current)
                    .map_err(|_| invalid_state(OPERATION, "stored summary revision is negative"))?,
            }),
            4 => Err(invalid_state(OPERATION, message)),
            other => Err(invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }
}

async fn clean_outbox_if_settled(
    client: Client,
    run_key: String,
    outbox_key: String,
    run_id: String,
    operation_timeout: Duration,
) -> Result<(), RunAggregationStoreError> {
    const OPERATION: &str = "aggregation_outbox_cleanup";
    let mut connection = run_redis_operation(
        OPERATION,
        operation_timeout,
        client.get_multiplexed_tokio_connection(),
    )
    .await?;
    let script = Script::new(CLEAN_OUTBOX_SCRIPT);
    let _: i64 = run_redis_operation(
        OPERATION,
        operation_timeout,
        script
            .key(run_key)
            .key(outbox_key)
            .arg(run_id)
            .invoke_async(&mut connection),
    )
    .await?;
    Ok(())
}

async fn run_redis_operation<T, F>(
    operation: &'static str,
    operation_timeout: Duration,
    future: F,
) -> Result<T, RunAggregationStoreError>
where
    F: Future<Output = redis::RedisResult<T>>,
{
    redis_operation(operation, operation_timeout, future)
        .await
        .map_err(map_coordination_error)
}

fn map_coordination_error(error: CoordinationError) -> RunAggregationStoreError {
    match error {
        CoordinationError::Unavailable { operation, message } => {
            RunAggregationStoreError::Unavailable { operation, message }
        }
        CoordinationError::Timeout { operation } => RunAggregationStoreError::Timeout { operation },
        CoordinationError::InvalidState { operation, message } => {
            RunAggregationStoreError::InvalidState { operation, message }
        }
        CoordinationError::StaleOwner { operation } => RunAggregationStoreError::InvalidState {
            operation,
            message: "unexpected stale-owner response".to_string(),
        },
    }
}

fn checked_duration_ms(
    operation: &'static str,
    duration: Duration,
) -> Result<i64, RunAggregationStoreError> {
    if duration.is_zero() {
        return Err(invalid_state(
            operation,
            "duration must be at least one millisecond",
        ));
    }
    i64::try_from(duration.as_millis())
        .map_err(|_| invalid_state(operation, "duration exceeds the Redis integer range"))
}

fn checked_lua_duration_ms(
    operation: &'static str,
    duration: Duration,
) -> Result<i64, RunAggregationStoreError> {
    let millis = checked_duration_ms(operation, duration)?;
    let unsigned = u64::try_from(millis)
        .map_err(|_| invalid_state(operation, "duration must not be negative"))?;
    if unsigned > MAX_EXACT_LUA_INTEGER {
        return Err(invalid_state(
            operation,
            "duration exceeds Redis Lua's exact integer range",
        ));
    }
    Ok(millis)
}

fn exact_result_integer(field: &'static str, value: u64) -> Result<(), RunAggregationStoreError> {
    if value > MAX_EXACT_LUA_INTEGER {
        return Err(RunAggregationStoreError::Contract(
            ContractError::InvalidResult(format!(
                "{field} exceeds Redis Lua's exact integer range"
            )),
        ));
    }
    Ok(())
}

fn validate_identity(field: &'static str, value: &str) -> Result<(), RunAggregationStoreError> {
    if value.trim().is_empty() || value.len() > MAX_IDENTITY_BYTES {
        return Err(invalid_state(
            "aggregation_ingest",
            format!("{field} must be non-empty and at most {MAX_IDENTITY_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn invalid_state(operation: &'static str, message: impl Into<String>) -> RunAggregationStoreError {
    RunAggregationStoreError::InvalidState {
        operation,
        message: message.into(),
    }
}

fn is_permanent_result_conflict(message: &str) -> bool {
    message == "scenario, schema, expected slices, or histogram bounds changed"
        || message == "slice index already belongs to a different execution identity"
        || message == "execution identity already belongs to a different slice"
        || message == "terminal run cannot accept an unrecorded slice"
}

fn snapshot_map(
    values: &[String],
    operation: &'static str,
) -> Result<HashMap<String, String>, RunAggregationStoreError> {
    if !values.len().is_multiple_of(2) {
        return Err(invalid_state(
            operation,
            "Redis aggregation snapshot has an odd field count",
        ));
    }
    Ok(values
        .chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect())
}

fn required<'a>(
    snapshot: &'a HashMap<String, String>,
    field: &'static str,
    operation: &'static str,
) -> Result<&'a str, RunAggregationStoreError> {
    snapshot
        .get(field)
        .map(String::as_str)
        .ok_or_else(|| invalid_state(operation, format!("aggregation field '{field}' is missing")))
}

fn parse_field<T: std::str::FromStr>(
    snapshot: &HashMap<String, String>,
    field: &'static str,
    operation: &'static str,
) -> Result<T, RunAggregationStoreError> {
    required(snapshot, field, operation)?
        .parse::<T>()
        .map_err(|_| invalid_state(operation, format!("aggregation field '{field}' is invalid")))
}

fn progress(
    values: &[String],
    operation: &'static str,
) -> Result<(u32, u32, &'static str), RunAggregationStoreError> {
    let snapshot = snapshot_map(values, operation)?;
    let received = parse_field(&snapshot, "received_slices", operation)?;
    let expected = parse_field(&snapshot, "expected_slices", operation)?;
    let status = required(&snapshot, "status", operation)?;
    // The returned status must outlive this local map, so translate it to one
    // of the static state labels rather than leaking Redis-owned text.
    let status = match status {
        "open" => "open",
        "partial" => "partial",
        "timed_out" => "timed_out",
        "cancelled" => "cancelled",
        "complete" => "complete",
        _ => return Err(invalid_state(operation, "aggregation status is invalid")),
    };
    Ok((received, expected, status))
}

fn finalized_status(
    status: &str,
    operation: &'static str,
) -> Result<Option<ScenarioRunSummaryStatus>, RunAggregationStoreError> {
    match status {
        "open" => Ok(None),
        "partial" => Ok(Some(ScenarioRunSummaryStatus::Partial)),
        "timed_out" => Ok(Some(ScenarioRunSummaryStatus::TimedOut)),
        "cancelled" => Ok(Some(ScenarioRunSummaryStatus::Cancelled)),
        "complete" => Ok(Some(ScenarioRunSummaryStatus::Complete)),
        _ => Err(invalid_state(operation, "aggregation status is invalid")),
    }
}

fn durable_summary_from_snapshot(
    values: &[String],
    operation: &'static str,
) -> Result<Option<DurableRunSummary>, RunAggregationStoreError> {
    if values.is_empty() {
        return Ok(None);
    }
    let snapshot = snapshot_map(values, operation)?;
    let status = required(&snapshot, "status", operation)?;
    if status == "open" {
        return Ok(None);
    }
    if status != "partial" && status != "timed_out" && status != "cancelled" && status != "complete"
    {
        return Err(invalid_state(operation, "aggregation status is invalid"));
    }
    let revision: u64 = parse_field(&snapshot, "summary_revision", operation)?;
    let pending_revision: u64 = parse_field(&snapshot, "summary_pending_revision", operation)?;
    let acknowledged_revision: u64 = parse_field(&snapshot, "summary_ack_revision", operation)?;
    if revision == 0 || pending_revision > revision || acknowledged_revision > revision {
        return Err(invalid_state(
            operation,
            "stored summary publication revisions are inconsistent",
        ));
    }
    let summary = summary_from_snapshot(values, operation)?;
    let status_label = match &summary.status {
        ScenarioRunSummaryStatus::Partial => "partial",
        ScenarioRunSummaryStatus::Complete => "complete",
        ScenarioRunSummaryStatus::TimedOut => "timed-out",
        ScenarioRunSummaryStatus::Cancelled => "cancelled",
    };
    Ok(Some(DurableRunSummary {
        revision,
        event_id: format!("{}:summary:r{revision}:{status_label}", summary.run_id),
        pending_publication: pending_revision > acknowledged_revision,
        summary,
    }))
}

fn summary_from_snapshot(
    values: &[String],
    operation: &'static str,
) -> Result<ScenarioRunSummary, RunAggregationStoreError> {
    let snapshot = snapshot_map(values, operation)?;
    let status = match required(&snapshot, "status", operation)? {
        "partial" => ScenarioRunSummaryStatus::Partial,
        "timed_out" => ScenarioRunSummaryStatus::TimedOut,
        "cancelled" => ScenarioRunSummaryStatus::Cancelled,
        "complete" => ScenarioRunSummaryStatus::Complete,
        other => {
            return Err(invalid_state(
                operation,
                format!("cannot build a final summary from status '{other}'"),
            ));
        }
    };
    let expected_slices: u32 = parse_field(&snapshot, "expected_slices", operation)?;
    let received_slices: u32 = parse_field(&snapshot, "received_slices", operation)?;
    if received_slices > expected_slices {
        return Err(invalid_state(
            operation,
            "received slice count exceeds expected slice count",
        ));
    }
    match status {
        ScenarioRunSummaryStatus::Complete if received_slices != expected_slices => {
            return Err(invalid_state(
                operation,
                "complete aggregation does not contain every expected slice",
            ));
        }
        ScenarioRunSummaryStatus::Partial
        | ScenarioRunSummaryStatus::TimedOut
        | ScenarioRunSummaryStatus::Cancelled
            if received_slices == expected_slices =>
        {
            return Err(invalid_state(
                operation,
                "incomplete aggregation status contains every expected slice",
            ));
        }
        _ => {}
    }

    let mut received_indexes = HashSet::new();
    for field in snapshot.keys() {
        if let Some(index) = field.strip_prefix("slice:") {
            let index = index.parse::<u32>().map_err(|_| {
                invalid_state(operation, "stored slice index is not an unsigned integer")
            })?;
            if index >= expected_slices {
                return Err(invalid_state(
                    operation,
                    "stored slice index exceeds expected slice count",
                ));
            }
            received_indexes.insert(index);
        }
    }
    if received_indexes.len() != received_slices as usize {
        return Err(invalid_state(
            operation,
            "stored received-slice count does not match slice identities",
        ));
    }
    let missing_slices = (0..expected_slices)
        .filter(|index| !received_indexes.contains(index))
        .collect();

    let histogram_bounds = required(&snapshot, "histogram_bounds", operation)?;
    let mut latency_histogram = Vec::new();
    if !histogram_bounds.is_empty() {
        let mut previous_bound = None;
        for (index, bound) in histogram_bounds.split(',').enumerate() {
            let upper_bound_ms = bound.parse::<u64>().map_err(|_| {
                invalid_state(operation, "stored latency histogram bound is invalid")
            })?;
            if previous_bound.is_some_and(|previous| upper_bound_ms <= previous) {
                return Err(invalid_state(
                    operation,
                    "stored latency histogram bounds are not strictly increasing",
                ));
            }
            previous_bound = Some(upper_bound_ms);
            let field = format!("latency:{index}");
            let count = snapshot
                .get(&field)
                .ok_or_else(|| invalid_state(operation, "latency bucket count is missing"))?
                .parse::<u64>()
                .map_err(|_| invalid_state(operation, "latency bucket count is invalid"))?;
            latency_histogram.push(LatencyBucket {
                upper_bound_ms,
                count,
            });
        }
    }

    let mut errors = BTreeMap::new();
    for (field, value) in &snapshot {
        if let Some(kind) = field.strip_prefix("error:") {
            let count = value
                .parse::<u64>()
                .map_err(|_| invalid_state(operation, "stored error count is invalid"))?;
            errors.insert(kind.to_string(), count);
        }
    }
    let error_breakdown: Vec<ErrorCount> = errors
        .into_iter()
        .map(|(kind, count)| ErrorCount { kind, count })
        .collect();
    let stored_error_kind_count: usize = parse_field(&snapshot, "error_kind_count", operation)?;
    if error_breakdown.len() != stored_error_kind_count {
        return Err(invalid_state(
            operation,
            "stored error-kind count does not match error fields",
        ));
    }

    let total: u64 = parse_field(&snapshot, "total", operation)?;
    let success: u64 = parse_field(&snapshot, "success", operation)?;
    let failure: u64 = parse_field(&snapshot, "failure", operation)?;
    if success.checked_add(failure) != Some(total) {
        return Err(invalid_state(
            operation,
            "stored success and failure counts do not equal total",
        ));
    }
    let histogram_total = latency_histogram.iter().try_fold(0_u64, |total, bucket| {
        total
            .checked_add(bucket.count)
            .ok_or_else(|| invalid_state(operation, "stored latency histogram count overflowed"))
    })?;
    if !latency_histogram.is_empty() && histogram_total != total {
        return Err(invalid_state(
            operation,
            "stored latency histogram count does not equal total",
        ));
    }

    Ok(ScenarioRunSummary {
        schema_version: parse_field(&snapshot, "schema_version", operation)?,
        scenario_id: required(&snapshot, "scenario_id", operation)?.to_string(),
        run_id: required(&snapshot, "run_id", operation)?.to_string(),
        status,
        expected_slices,
        received_slices,
        missing_slices,
        total,
        success,
        failure,
        scenario_latency_p50_ms: histogram_quantile(&latency_histogram, 0.50),
        scenario_latency_p95_ms: histogram_quantile(&latency_histogram, 0.95),
        scenario_latency_p99_ms: histogram_quantile(&latency_histogram, 0.99),
        latency_histogram,
        error_breakdown,
        first_result_at_unix_ms: parse_field(&snapshot, "first_result_at", operation)?,
        finalized_at_unix_ms: parse_field(&snapshot, "finalized_at", operation)?,
    })
}

fn histogram_quantile(histogram: &[LatencyBucket], quantile: f64) -> u64 {
    let total: u64 = histogram.iter().map(|bucket| bucket.count).sum();
    if total == 0 {
        return 0;
    }
    let rank = ((total as f64) * quantile).ceil().max(1.0) as u64;
    let mut cumulative = 0_u64;
    let mut previous_bound = 0_u64;
    for bucket in histogram {
        cumulative = cumulative.saturating_add(bucket.count);
        if cumulative >= rank {
            return if bucket.upper_bound_ms == u64::MAX {
                previous_bound
            } else {
                bucket.upper_bound_ms
            };
        }
        previous_bound = bucket.upper_bound_ms;
    }
    previous_bound
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::contracts::{
        CURRENT_CONTRACT_VERSION, ScenarioRunStatus, build_terminal_event_id,
    };
    use crate::domain::error::ContractError;

    fn result(error_breakdown: Vec<ErrorCount>) -> ScenarioRunResult {
        let execution_key = "run:slice-0-of-2".to_string();
        let failure = error_breakdown.iter().map(|error| error.count).sum::<u64>();
        ScenarioRunResult {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "checkout".to_string(),
            run_id: "run".to_string(),
            event_id: build_terminal_event_id(&execution_key, 0, "result"),
            attempt: 0,
            execution_key,
            slice: crate::domain::contracts::JobSlice { index: 0, total: 2 },
            started_at_unix_ms: 1,
            finished_at_unix_ms: 2,
            status: if failure == 0 {
                ScenarioRunStatus::Success
            } else {
                ScenarioRunStatus::Failed
            },
            total: 3,
            success: 3 - failure,
            failure,
            scenario_latency_p50_ms: 10,
            scenario_latency_p95_ms: 100,
            scenario_latency_p99_ms: 100,
            latency_histogram: vec![
                LatencyBucket {
                    upper_bound_ms: 10,
                    count: 2,
                },
                LatencyBucket {
                    upper_bound_ms: 100,
                    count: 1,
                },
            ],
            error_breakdown,
        }
    }

    fn store(max_error_kinds: usize) -> RedisRunAggregationStore {
        RedisRunAggregationStore::new(
            Client::open("redis://127.0.0.1:1").unwrap(),
            "test:aggregate".to_string(),
            Duration::from_secs(10),
            Duration::from_secs(60),
        )
        .unwrap()
        .with_max_error_kinds(max_error_kinds)
        .unwrap()
    }

    #[test]
    fn normalization_rejects_duplicate_error_kinds_as_contract_input() {
        let error = store(4)
            .normalize_result(&result(vec![
                ErrorCount {
                    kind: "target".to_string(),
                    count: 1,
                },
                ErrorCount {
                    kind: "target".to_string(),
                    count: 2,
                },
            ]))
            .unwrap_err();
        assert!(matches!(
            error,
            RunAggregationStoreError::Contract(ContractError::InvalidResult(_))
        ));
    }

    #[test]
    fn normalization_enforces_bounded_cardinality_and_exact_counters() {
        let capacity_error = store(1)
            .normalize_result(&result(vec![
                ErrorCount {
                    kind: "one".to_string(),
                    count: 1,
                },
                ErrorCount {
                    kind: "two".to_string(),
                    count: 1,
                },
            ]))
            .unwrap_err();
        assert!(matches!(
            capacity_error,
            RunAggregationStoreError::ErrorKindCapacity { max_error_kinds: 1 }
        ));

        let mut oversized = result(Vec::new());
        // Legacy v1 contracts did not impose Redis Lua's exact-integer bound.
        // The record is nevertheless a permanent input failure, not corrupted
        // Redis state or a transient dependency outage.
        oversized.schema_version = 1;
        oversized.event_id.clear();
        oversized.total = MAX_EXACT_LUA_INTEGER + 1;
        oversized.success = oversized.total;
        oversized.latency_histogram[0].count = oversized.total;
        oversized.latency_histogram[1].count = 0;
        assert!(matches!(
            store(4).normalize_result(&oversized),
            Err(RunAggregationStoreError::Contract(
                ContractError::InvalidResult(_)
            ))
        ));
    }

    #[test]
    fn snapshot_summary_detects_missing_slices_and_merges_histogram() {
        let snapshot = vec![
            "scenario_id",
            "checkout",
            "run_id",
            "run",
            "schema_version",
            "2",
            "expected_slices",
            "2",
            "received_slices",
            "1",
            "status",
            "partial",
            "total",
            "3",
            "success",
            "2",
            "failure",
            "1",
            "histogram_bounds",
            "10,100",
            "latency:0",
            "2",
            "latency:1",
            "1",
            "error_kind_count",
            "1",
            "error:target",
            "1",
            "slice:1",
            "execution-1",
            "first_result_at",
            "100",
            "finalized_at",
            "220",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let summary = summary_from_snapshot(&snapshot, "test").unwrap();
        assert_eq!(summary.status, ScenarioRunSummaryStatus::Partial);
        assert_eq!(summary.missing_slices, vec![0]);
        assert_eq!(summary.scenario_latency_p50_ms, 10);
        assert_eq!(summary.scenario_latency_p95_ms, 100);
        assert_eq!(
            summary.error_breakdown,
            vec![ErrorCount {
                kind: "target".to_string(),
                count: 1,
            }]
        );
    }

    #[test]
    fn durable_snapshot_exposes_deterministic_revision_and_outbox_state() {
        let snapshot = vec![
            "scenario_id",
            "checkout",
            "run_id",
            "run",
            "schema_version",
            "2",
            "expected_slices",
            "2",
            "received_slices",
            "1",
            "status",
            "timed_out",
            "total",
            "1",
            "success",
            "1",
            "failure",
            "0",
            "histogram_bounds",
            "10",
            "latency:0",
            "1",
            "error_kind_count",
            "0",
            "slice:0",
            "execution-0",
            "first_result_at",
            "100",
            "finalized_at",
            "220",
            "summary_revision",
            "2",
            "summary_pending_revision",
            "2",
            "summary_ack_revision",
            "1",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let durable = durable_summary_from_snapshot(&snapshot, "test")
            .unwrap()
            .expect("finalized summary");
        assert_eq!(durable.revision, 2);
        assert_eq!(durable.event_id, "run:summary:r2:timed-out");
        assert!(durable.pending_publication);
        assert_eq!(durable.summary.status, ScenarioRunSummaryStatus::TimedOut);
    }

    #[test]
    fn configuration_and_scan_bounds_fail_fast() {
        let client = Client::open("redis://127.0.0.1:1").unwrap();
        assert!(matches!(
            RedisRunAggregationStore::new(
                client.clone(),
                "test:{caller-controlled}".to_string(),
                Duration::from_secs(1),
                Duration::from_secs(60),
            ),
            Err(RunAggregationStoreError::InvalidState { .. })
        ));
        assert!(matches!(
            RedisRunAggregationStore::new(
                client.clone(),
                "test".to_string(),
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
            Err(RunAggregationStoreError::InvalidState { .. })
        ));
        let store = RedisRunAggregationStore::new(
            client,
            "test".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(60),
        )
        .unwrap()
        .with_max_scan_limit(4)
        .unwrap();
        assert!(store.validate_scan_limit(0).is_err());
        assert!(store.validate_scan_limit(5).is_err());
        assert_eq!(store.validate_scan_limit(4), Ok(4));
    }

    #[test]
    fn every_multi_key_aggregation_operation_uses_the_runs_hash_tag() {
        fn hash_tag(key: &str) -> Option<&str> {
            let (_, suffix) = key.split_once('{')?;
            let (tag, _) = suffix.split_once('}')?;
            (!tag.is_empty()).then_some(tag)
        }

        let client = Client::open("redis://127.0.0.1:1").unwrap();
        let store = RedisRunAggregationStore::new(
            client,
            "pulse:test:aggregation".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(60),
        )
        .unwrap();
        let keys = [
            store.key_for_run("run-1"),
            store.deadline_key(),
            store.outbox_key(),
            store.active_key(),
        ];
        assert!(
            keys.iter().all(|key| hash_tag(key) == Some("runs")),
            "all aggregation script keys must resolve to the same Redis Cluster slot: {keys:?}"
        );
    }
}
