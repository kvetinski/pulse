use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::{Client, Script};
use tokio::time::timeout;
use uuid::Uuid;

use crate::domain::coordination::{
    ClaimOutcome, CompletedOutcome, CoordinationError, DispatchOutcome, DispatchProgress,
    DispatchSpec, DispatchStore, DispatchWindow, ExecutionClaim, ExecutionLease,
    ExecutionLeaseStore, LeaderElector, LeaderLease, LeadershipOutcome, ReleaseOutcome,
    TerminalOutcome,
};
use crate::domain::scenario::RepeatPolicy;

mod aggregation;
pub use aggregation::RedisRunAggregationStore;

const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_EXECUTION_LEASE_TTL: Duration = Duration::from_secs(30);
const MAX_DISPATCH_SLICES: u32 = 4_096;

async fn redis_operation<T, F>(
    operation: &'static str,
    operation_timeout: Duration,
    future: F,
) -> Result<T, CoordinationError>
where
    F: Future<Output = redis::RedisResult<T>>,
{
    match timeout(operation_timeout, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(CoordinationError::unavailable(operation, error.to_string())),
        Err(_) => Err(CoordinationError::Timeout { operation }),
    }
}

fn duration_ms(operation: &'static str, duration: Duration) -> Result<i64, CoordinationError> {
    let millis = duration.as_millis();
    if millis == 0 {
        return Err(CoordinationError::invalid_state(
            operation,
            "duration must be at least one millisecond",
        ));
    }
    i64::try_from(millis).map_err(|_| {
        CoordinationError::invalid_state(operation, "duration exceeds Redis integer range")
    })
}

fn duration_from_nonnegative_ms(value: i64) -> Duration {
    Duration::from_millis(u64::try_from(value.max(0)).unwrap_or(u64::MAX))
}

fn as_u64(
    operation: &'static str,
    field: &'static str,
    value: i64,
) -> Result<u64, CoordinationError> {
    u64::try_from(value).map_err(|_| {
        CoordinationError::invalid_state(operation, format!("{field} must not be negative"))
    })
}

pub struct RedisLeaderElector {
    client: Client,
    lock_key: String,
    fence_key: String,
    node_id: String,
    lock_ttl: Duration,
    operation_timeout: Duration,
}

impl RedisLeaderElector {
    pub fn new(client: Client, lock_key: String, node_id: String, lock_ttl_ms: u64) -> Self {
        let fence_key = format!("{lock_key}:fence");
        Self {
            client,
            lock_key,
            fence_key,
            node_id,
            lock_ttl: Duration::from_millis(lock_ttl_ms),
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
        }
    }

    pub fn with_operation_timeout(mut self, operation_timeout: Duration) -> Self {
        self.operation_timeout = operation_timeout;
        self
    }

    async fn acquire(&self) -> Result<LeadershipOutcome, CoordinationError> {
        const OPERATION: &str = "leader_acquire";
        let ttl_ms = duration_ms(OPERATION, self.lock_ttl)?;
        let owner_token = format!("{}:{}", self.node_id, Uuid::new_v4());
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;

        let script = Script::new(
            r#"
            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)

            if redis.call('EXISTS', KEYS[1]) == 0 then
                local fence = redis.call('INCR', KEYS[2])
                redis.call('HSET', KEYS[1],
                    'owner', ARGV[1],
                    'node_id', ARGV[2],
                    'fence', tostring(fence))
                redis.call('PEXPIRE', KEYS[1], ARGV[3])
                return {1, fence, now_ms + tonumber(ARGV[3]), 0, ''}
            end

            local remaining = redis.call('PTTL', KEYS[1])
            if remaining < 0 then
                return {3, 0, 0, 0, 'leader record has no expiry'}
            end
            return {2, 0, 0, remaining, ''}
            "#,
        );

        let (code, fence, expires_at, retry_after, message): (i64, i64, i64, i64, String) =
            redis_operation(
                OPERATION,
                self.operation_timeout,
                script
                    .key(&self.lock_key)
                    .key(&self.fence_key)
                    .arg(&owner_token)
                    .arg(&self.node_id)
                    .arg(ttl_ms)
                    .invoke_async(&mut connection),
            )
            .await?;

        match code {
            1 => Ok(LeadershipOutcome::Acquired(LeaderLease {
                lock_key: self.lock_key.clone(),
                node_id: self.node_id.clone(),
                owner_token,
                fencing_token: as_u64(OPERATION, "fencing token", fence)?,
                expires_at_unix_ms: as_u64(OPERATION, "lease expiry", expires_at)?,
                ttl: self.lock_ttl,
            })),
            2 => Ok(LeadershipOutcome::Follower {
                retry_after: duration_from_nonnegative_ms(retry_after),
            }),
            3 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn renew(&self, current: &LeaderLease) -> Result<LeadershipOutcome, CoordinationError> {
        const OPERATION: &str = "leader_renew";
        if current.lock_key != self.lock_key || current.node_id != self.node_id {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "lease belongs to a different elector",
            ));
        }

        let ttl_ms = duration_ms(OPERATION, self.lock_ttl)?;
        let fence = i64::try_from(current.fencing_token).map_err(|_| {
            CoordinationError::invalid_state(OPERATION, "fencing token exceeds Redis integer range")
        })?;
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;

        let script = Script::new(
            r#"
            if redis.call('EXISTS', KEYS[1]) == 0 then
                return {2, 0, 'leader record is absent'}
            end
            if redis.call('HGET', KEYS[1], 'owner') ~= ARGV[1]
                or redis.call('HGET', KEYS[1], 'fence') ~= ARGV[2] then
                return {2, 0, 'leader owner or fence changed'}
            end

            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            redis.call('PEXPIRE', KEYS[1], ARGV[3])
            return {1, now_ms + tonumber(ARGV[3]), ''}
            "#,
        );

        let (code, expires_at, message): (i64, i64, String) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(&self.lock_key)
                .arg(&current.owner_token)
                .arg(fence)
                .arg(ttl_ms)
                .invoke_async(&mut connection),
        )
        .await?;

        match code {
            1 => {
                let mut renewed = current.clone();
                renewed.expires_at_unix_ms = as_u64(OPERATION, "lease expiry", expires_at)?;
                renewed.ttl = self.lock_ttl;
                Ok(LeadershipOutcome::Renewed(renewed))
            }
            2 => Err(CoordinationError::stale_owner(OPERATION)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}: {message}"),
            )),
        }
    }
}

#[async_trait]
impl LeaderElector for RedisLeaderElector {
    async fn acquire_or_renew(
        &self,
        current: Option<&LeaderLease>,
    ) -> Result<LeadershipOutcome, CoordinationError> {
        match current {
            Some(lease) => self.renew(lease).await,
            None => self.acquire().await,
        }
    }

    async fn relinquish(&self, lease: &LeaderLease) -> Result<(), CoordinationError> {
        const OPERATION: &str = "leader_relinquish";
        if lease.lock_key != self.lock_key || lease.node_id != self.node_id {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "lease belongs to a different elector",
            ));
        }
        let fence = i64::try_from(lease.fencing_token).map_err(|_| {
            CoordinationError::invalid_state(OPERATION, "fencing token exceeds Redis integer range")
        })?;
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(
            r#"
            if redis.call('EXISTS', KEYS[1]) == 0 then
                return 0
            end
            if redis.call('HGET', KEYS[1], 'owner') ~= ARGV[1]
                or redis.call('HGET', KEYS[1], 'fence') ~= ARGV[2] then
                return 2
            end
            redis.call('DEL', KEYS[1])
            return 1
            "#,
        );
        let code: i64 = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(&self.lock_key)
                .arg(&lease.owner_token)
                .arg(fence)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            0 | 1 => Ok(()),
            2 => Err(CoordinationError::stale_owner(OPERATION)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }
}

pub struct RedisDueStateStore {
    client: Client,
    schedule_prefix: String,
    operation_timeout: Duration,
    aggregation_registration: Option<AggregationRegistrationConfig>,
}

struct AggregationRegistrationConfig {
    key_prefix: String,
    partial_timeout: Duration,
    retention: Duration,
    max_active_runs: usize,
}

impl RedisDueStateStore {
    pub fn new(client: Client, schedule_prefix: String) -> Self {
        Self {
            client,
            schedule_prefix,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            aggregation_registration: None,
        }
    }

    pub fn with_operation_timeout(mut self, operation_timeout: Duration) -> Self {
        self.operation_timeout = operation_timeout;
        self
    }

    pub fn with_aggregation_registration(
        mut self,
        key_prefix: String,
        partial_timeout: Duration,
        retention: Duration,
        max_active_runs: usize,
    ) -> Self {
        self.aggregation_registration = Some(AggregationRegistrationConfig {
            key_prefix,
            partial_timeout,
            retention,
            max_active_runs,
        });
        self
    }

    fn key_for(&self, scenario_id: &str) -> String {
        format!("{}:{}", self.schedule_prefix, scenario_id)
    }
}

fn parse_missing_slices(
    operation: &'static str,
    raw: &str,
    total_slices: u32,
) -> Result<Vec<u32>, CoordinationError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut slices = Vec::new();
    for value in raw.split(',') {
        let index = value.parse::<u32>().map_err(|_| {
            CoordinationError::invalid_state(
                operation,
                format!("invalid missing-slice index '{value}'"),
            )
        })?;
        if index >= total_slices || slices.last().is_some_and(|previous| *previous >= index) {
            return Err(CoordinationError::invalid_state(
                operation,
                "missing-slice indexes are out of range or not strictly ordered",
            ));
        }
        slices.push(index);
    }
    Ok(slices)
}

#[async_trait]
impl DispatchStore for RedisDueStateStore {
    async fn prepare_window(
        &self,
        spec: &DispatchSpec,
        leader: &LeaderLease,
    ) -> Result<DispatchOutcome, CoordinationError> {
        const OPERATION: &str = "dispatch_prepare_window";
        if spec.scenario_id.trim().is_empty() {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "scenario id must not be empty",
            ));
        }
        if spec.contract_version == 0 {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "contract version must be positive",
            ));
        }
        if spec.total_slices == 0 || spec.total_slices > MAX_DISPATCH_SLICES {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                format!("total slices must be between 1 and {MAX_DISPATCH_SLICES}"),
            ));
        }
        if spec.plan_fingerprint.trim().is_empty() {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "plan fingerprint must not be empty",
            ));
        }

        let (repeat_kind, repeat_ms) = match &spec.repeat {
            RepeatPolicy::Once => ("once", 0_i64),
            RepeatPolicy::Every(interval) => ("every", duration_ms(OPERATION, *interval)?),
        };
        let fence = i64::try_from(leader.fencing_token).map_err(|_| {
            CoordinationError::invalid_state(OPERATION, "fencing token exceeds Redis integer range")
        })?;
        let schedule_key = self.key_for(&spec.scenario_id);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;

        let script = Script::new(
            r#"
            local function leader_is_current()
                if redis.call('EXISTS', KEYS[1]) == 0 then
                    return false
                end
                if redis.call('PTTL', KEYS[1]) <= 0 then
                    return false
                end
                return redis.call('HGET', KEYS[1], 'owner') == ARGV[1]
                    and redis.call('HGET', KEYS[1], 'fence') == ARGV[2]
            end

            if not leader_is_current() then
                return {4, '', 0, '', 0, 'leader lease is stale'}
            end

            local scenario_id = ARGV[3]
            local contract_version = ARGV[4]
            local total_slices = tonumber(ARGV[5])
            local repeat_kind = ARGV[6]
            local repeat_ms = tonumber(ARGV[7])
            local active_window = redis.call('HGET', KEYS[2], 'active_window_id')

            if active_window then
                if redis.call('HGET', KEYS[2], 'active_contract_version') ~= contract_version
                    or tonumber(redis.call('HGET', KEYS[2], 'active_total_slices')) ~= total_slices
                    or redis.call('HGET', KEYS[2], 'active_repeat_kind') ~= repeat_kind
                    or tonumber(redis.call('HGET', KEYS[2], 'active_repeat_ms')) ~= repeat_ms
                    or redis.call('HGET', KEYS[2], 'active_plan_fingerprint') ~= ARGV[8] then
                    return {5, '', 0, '', 0, 'active window metadata does not match current dispatch spec'}
                end

                local scheduled_at = tonumber(redis.call('HGET', KEYS[2], 'active_scheduled_at'))
                local missing = {}
                local acked = 0
                for index = 0, total_slices - 1 do
                    if redis.call('HGET', KEYS[2], 'ack:' .. tostring(index)) == '1' then
                        acked = acked + 1
                    else
                        table.insert(missing, tostring(index))
                    end
                end
                if tonumber(redis.call('HGET', KEYS[2], 'active_ack_count') or '-1') ~= acked then
                    return {5, '', 0, '', 0, 'active acknowledgement count is inconsistent'}
                end
                return {1, active_window, scheduled_at, table.concat(missing, ','), 0, ''}
            end

            if repeat_kind == 'once' and redis.call('HGET', KEYS[2], 'once_done') == '1' then
                return {3, '', 0, '', 0, ''}
            end

            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            local next_at = tonumber(redis.call('HGET', KEYS[2], 'next_at') or tostring(now_ms))
            if next_at > now_ms then
                return {2, '', 0, '', next_at - now_ms, ''}
            end

            local scheduled_at = next_at
            local window_id = 'v' .. contract_version
                .. ':s' .. tostring(string.len(scenario_id)) .. ':' .. scenario_id
                .. ':w' .. tostring(scheduled_at)
                .. ':n' .. tostring(total_slices)
            redis.call('HSET', KEYS[2],
                'active_window_id', window_id,
                'active_scheduled_at', tostring(scheduled_at),
                'active_contract_version', contract_version,
                'active_total_slices', tostring(total_slices),
                'active_repeat_kind', repeat_kind,
                'active_repeat_ms', tostring(repeat_ms),
                'active_plan_fingerprint', ARGV[8],
                'active_ack_count', '0')

            local missing = {}
            for index = 0, total_slices - 1 do
                table.insert(missing, tostring(index))
            end
            return {1, window_id, scheduled_at, table.concat(missing, ','), 0, ''}
            "#,
        );

        let (code, window_id, scheduled_at, missing_raw, retry_after, message): (
            i64,
            String,
            i64,
            String,
            i64,
            String,
        ) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(&leader.lock_key)
                .key(schedule_key)
                .arg(&leader.owner_token)
                .arg(fence)
                .arg(&spec.scenario_id)
                .arg(spec.contract_version)
                .arg(spec.total_slices)
                .arg(repeat_kind)
                .arg(repeat_ms)
                .arg(&spec.plan_fingerprint)
                .invoke_async(&mut connection),
        )
        .await?;

        match code {
            1 => {
                let missing_slices =
                    parse_missing_slices(OPERATION, &missing_raw, spec.total_slices)?;
                if missing_slices.is_empty() {
                    return Err(CoordinationError::invalid_state(
                        OPERATION,
                        "active window has no missing slices but is not complete",
                    ));
                }
                let scheduled_at_unix_ms =
                    u128::from(as_u64(OPERATION, "scheduled timestamp", scheduled_at)?);
                Ok(DispatchOutcome::Ready(DispatchWindow {
                    scenario_id: spec.scenario_id.clone(),
                    run_id: window_id.clone(),
                    window_id,
                    scheduled_at_unix_ms,
                    contract_version: spec.contract_version,
                    total_slices: spec.total_slices,
                    plan_fingerprint: spec.plan_fingerprint.clone(),
                    missing_slices,
                }))
            }
            2 => Ok(DispatchOutcome::NotDue {
                retry_after: duration_from_nonnegative_ms(retry_after),
            }),
            3 => Ok(DispatchOutcome::Finished),
            4 => Err(CoordinationError::stale_owner(OPERATION)),
            5 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn ack_slice(
        &self,
        window: &DispatchWindow,
        slice_index: u32,
        leader: &LeaderLease,
    ) -> Result<DispatchProgress, CoordinationError> {
        const OPERATION: &str = "dispatch_ack_slice";
        if slice_index >= window.total_slices {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "slice index is outside the dispatch window",
            ));
        }
        if window.total_slices == 0 || window.total_slices > MAX_DISPATCH_SLICES {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "dispatch window has an invalid slice count",
            ));
        }
        let fence = i64::try_from(leader.fencing_token).map_err(|_| {
            CoordinationError::invalid_state(OPERATION, "fencing token exceeds Redis integer range")
        })?;
        let schedule_key = self.key_for(&window.scenario_id);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;

        let script = Script::new(
            r#"
            local function leader_is_current()
                if redis.call('EXISTS', KEYS[1]) == 0 then
                    return false
                end
                if redis.call('PTTL', KEYS[1]) <= 0 then
                    return false
                end
                return redis.call('HGET', KEYS[1], 'owner') == ARGV[1]
                    and redis.call('HGET', KEYS[1], 'fence') == ARGV[2]
            end

            if not leader_is_current() then
                return {4, 0, 'leader lease is stale'}
            end

            local active_window = redis.call('HGET', KEYS[2], 'active_window_id')
            if not active_window then
                if redis.call('HGET', KEYS[2], 'last_completed_window') == ARGV[3] then
                    return {2, 0, ''}
                end
                return {5, 0, 'dispatch window is not active'}
            end
            if active_window ~= ARGV[3] then
                return {5, 0, 'a different dispatch window is active'}
            end

            local total_slices = tonumber(redis.call('HGET', KEYS[2], 'active_total_slices'))
            if total_slices ~= tonumber(ARGV[5]) then
                return {5, 0, 'dispatch slice count changed'}
            end
            if redis.call('HGET', KEYS[2], 'active_plan_fingerprint') ~= ARGV[6] then
                return {5, 0, 'dispatch plan fingerprint changed'}
            end
            local slice_index = tonumber(ARGV[4])
            if slice_index < 0 or slice_index >= total_slices then
                return {5, 0, 'slice index is outside the active window'}
            end

            local repeat_kind = redis.call('HGET', KEYS[2], 'active_repeat_kind')
            local repeat_ms = tonumber(redis.call('HGET', KEYS[2], 'active_repeat_ms') or '0')
            local scheduled_at = tonumber(redis.call('HGET', KEYS[2], 'active_scheduled_at'))
            if repeat_kind ~= 'once' and not (repeat_kind == 'every' and repeat_ms > 0) then
                return {5, 0, 'active repeat policy is invalid'}
            end
            if not scheduled_at then
                return {5, 0, 'active schedule timestamp is invalid'}
            end

            local previous_ack_count = tonumber(redis.call('HGET', KEYS[2], 'active_ack_count') or '-1')
            if previous_ack_count < 0 or previous_ack_count > total_slices then
                return {5, 0, 'acknowledgement count is invalid'}
            end

            local ack_field = 'ack:' .. tostring(slice_index)
            if redis.call('HSETNX', KEYS[2], ack_field, '1') == 1 then
                redis.call('HINCRBY', KEYS[2], 'active_ack_count', 1)
            end
            local ack_count = tonumber(redis.call('HGET', KEYS[2], 'active_ack_count') or '0')
            local remaining = total_slices - ack_count
            if remaining > 0 then
                return {1, remaining, ''}
            end
            if remaining < 0 then
                return {5, 0, 'acknowledgement count exceeds slice count'}
            end

            if repeat_kind == 'once' then
                redis.call('HSET', KEYS[2], 'once_done', '1')
                redis.call('HDEL', KEYS[2], 'next_at')
            else
                redis.call('HSET', KEYS[2], 'next_at', tostring(scheduled_at + repeat_ms))
            end

            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            redis.call('HSET', KEYS[2],
                'last_completed_window', active_window,
                'last_completed_at', tostring(now_ms))
            for index = 0, total_slices - 1 do
                redis.call('HDEL', KEYS[2], 'ack:' .. tostring(index))
            end
            redis.call('HDEL', KEYS[2],
                'active_window_id',
                'active_scheduled_at',
                'active_contract_version',
                'active_total_slices',
                'active_repeat_kind',
                'active_repeat_ms',
                'active_plan_fingerprint',
                'active_ack_count')
            return {2, 0, ''}
            "#,
        );

        let (code, remaining, message): (i64, i64, String) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(&leader.lock_key)
                .key(schedule_key)
                .arg(&leader.owner_token)
                .arg(fence)
                .arg(&window.window_id)
                .arg(slice_index)
                .arg(window.total_slices)
                .arg(&window.plan_fingerprint)
                .invoke_async(&mut connection),
        )
        .await?;

        match code {
            1 => Ok(DispatchProgress::Pending {
                remaining_slices: u32::try_from(remaining).map_err(|_| {
                    CoordinationError::invalid_state(
                        OPERATION,
                        "remaining slice count is out of range",
                    )
                })?,
            }),
            2 => Ok(DispatchProgress::Complete),
            4 => Err(CoordinationError::stale_owner(OPERATION)),
            5 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn register_run(
        &self,
        window: &DispatchWindow,
        load_duration: Duration,
    ) -> Result<(), CoordinationError> {
        const OPERATION: &str = "aggregation_register_run";
        let Some(config) = &self.aggregation_registration else {
            return Ok(());
        };
        let load_ms = u128::from(u64::try_from(load_duration.as_millis()).map_err(|_| {
            CoordinationError::invalid_state(OPERATION, "load duration exceeds u64 milliseconds")
        })?);
        let partial_ms = u128::from(u64::try_from(config.partial_timeout.as_millis()).map_err(
            |_| {
                CoordinationError::invalid_state(
                    OPERATION,
                    "partial timeout exceeds u64 milliseconds",
                )
            },
        )?);
        let deadline = window
            .scheduled_at_unix_ms
            .checked_add(load_ms)
            .and_then(|value| value.checked_add(partial_ms))
            .ok_or_else(|| {
                CoordinationError::invalid_state(OPERATION, "run deadline overflowed")
            })?;
        const MAX_EXACT_LUA_INTEGER: u128 = 9_007_199_254_740_991;
        if deadline > MAX_EXACT_LUA_INTEGER {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "run deadline exceeds Redis Lua's exact integer range",
            ));
        }
        let retention_ms = duration_ms(OPERATION, config.retention)?;
        let max_active = i64::try_from(config.max_active_runs).map_err(|_| {
            CoordinationError::invalid_state(OPERATION, "active-run limit exceeds Redis range")
        })?;
        if max_active < 1 {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "active-run limit must be positive",
            ));
        }
        let run_key = format!("{}:{{runs}}:run:{}", config.key_prefix, window.run_id);
        let deadline_key = format!("{}:{{runs}}:deadlines", config.key_prefix);
        let active_key = format!("{}:{{runs}}:active", config.key_prefix);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(
            r#"
            local max_exact = 9007199254740991
            local run_id = ARGV[1]
            local scenario_id = ARGV[2]
            local schema_version = ARGV[3]
            local expected_slices = tonumber(ARGV[4])
            local deadline_at = tonumber(ARGV[5])
            local retention_ms = tonumber(ARGV[6])
            local max_active = tonumber(ARGV[7])
            if not expected_slices or expected_slices < 1
                or not deadline_at or deadline_at < 0 or deadline_at > max_exact
                or not retention_ms or retention_ms < 1
                or not max_active or max_active < 1 then
                return {3, 'invalid run registration arguments'}
            end
            local now_parts = redis.call('TIME')
            local redis_now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', redis_now_ms)
            if redis.call('EXISTS', KEYS[1]) == 1 then
                if redis.call('HGET', KEYS[1], 'run_id') ~= run_id
                    or redis.call('HGET', KEYS[1], 'scenario_id') ~= scenario_id
                    or redis.call('HGET', KEYS[1], 'schema_version') ~= schema_version
                    or tonumber(redis.call('HGET', KEYS[1], 'expected_slices') or '-1') ~= expected_slices then
                    return {3, 'registered run metadata changed'}
                end
                local pending = tonumber(redis.call('HGET', KEYS[1], 'summary_pending_revision') or '0')
                local acknowledged = tonumber(redis.call('HGET', KEYS[1], 'summary_ack_revision') or '0')
                if pending > acknowledged then
                    redis.call('PERSIST', KEYS[1])
                    redis.call('ZADD', KEYS[3], max_exact, run_id)
                else
                    redis.call('PEXPIRE', KEYS[1], retention_ms)
                    redis.call('ZADD', KEYS[3], redis_now_ms + retention_ms, run_id)
                end
                return {2, ''}
            end
            if redis.call('ZCARD', KEYS[3]) >= max_active then
                return {4, 'bounded active-run capacity exceeded'}
            end
            redis.call('HSET', KEYS[1],
                'scenario_id', scenario_id,
                'run_id', run_id,
                'schema_version', schema_version,
                'expected_slices', tostring(expected_slices),
                'histogram_bounds', '',
                'status', 'open',
                'received_slices', '0',
                'total', '0',
                'success', '0',
                'failure', '0',
                'error_kind_count', '0',
                'first_result_at', '0',
                'last_result_at', '0',
                'deadline_at', tostring(deadline_at),
                'finalized_at', '0',
                'summary_revision', '0',
                'summary_pending_revision', '0',
                'summary_ack_revision', '0')
            redis.call('ZADD', KEYS[2], deadline_at, run_id)
            redis.call('PEXPIRE', KEYS[1], retention_ms)
            redis.call('ZADD', KEYS[3], redis_now_ms + retention_ms, run_id)
            return {1, ''}
            "#,
        );
        let (code, message): (i64, String) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(run_key)
                .key(deadline_key)
                .key(active_key)
                .arg(&window.run_id)
                .arg(&window.scenario_id)
                .arg(window.contract_version)
                .arg(window.total_slices)
                .arg(deadline.to_string())
                .arg(retention_ms)
                .arg(max_active)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            1 | 2 => Ok(()),
            3 | 4 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }
}

pub struct RedisIdempotencyStore {
    client: Client,
    key_prefix: String,
    lease_ttl: Duration,
    terminal_ttl: Duration,
    operation_timeout: Duration,
}

impl RedisIdempotencyStore {
    /// Compatibility constructor: `ttl` is terminal outcome retention. Running
    /// leases are capped at 30 seconds and must be renewed by the worker.
    pub fn new(client: Client, key_prefix: String, ttl: Duration) -> Self {
        Self {
            client,
            key_prefix,
            lease_ttl: ttl.min(DEFAULT_EXECUTION_LEASE_TTL),
            terminal_ttl: ttl,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
        }
    }

    pub fn with_timings(
        client: Client,
        key_prefix: String,
        lease_ttl: Duration,
        terminal_ttl: Duration,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            client,
            key_prefix,
            lease_ttl,
            terminal_ttl,
            operation_timeout,
        }
    }

    pub fn lease_ttl(&self) -> Duration {
        self.lease_ttl
    }

    fn key_for(&self, claim: &ExecutionClaim) -> String {
        format!(
            "{}:{}:attempt-{}",
            self.key_prefix, claim.execution_key, claim.attempt
        )
    }

    fn key_for_lease(&self, lease: &ExecutionLease) -> String {
        format!(
            "{}:{}:attempt-{}",
            self.key_prefix, lease.execution_key, lease.attempt
        )
    }

    fn running_retention(&self) -> Duration {
        self.terminal_ttl.max(self.lease_ttl.saturating_mul(4))
    }
}

#[async_trait]
impl ExecutionLeaseStore for RedisIdempotencyStore {
    async fn claim(&self, claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError> {
        const OPERATION: &str = "execution_claim";
        if claim.execution_key.trim().is_empty() {
            return Err(CoordinationError::invalid_state(
                OPERATION,
                "execution key must not be empty",
            ));
        }
        let lease_ttl_ms = duration_ms(OPERATION, self.lease_ttl)?;
        let running_retention_ms = duration_ms(OPERATION, self.running_retention())?;
        let owner_token = Uuid::new_v4().to_string();
        let key = self.key_for(claim);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;

        let script = Script::new(
            r#"
            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            local lease_until = now_ms + tonumber(ARGV[4])

            if redis.call('EXISTS', KEYS[1]) == 0 then
                redis.call('HSET', KEYS[1],
                    'state', 'running',
                    'owner', ARGV[1],
                    'execution_key', ARGV[2],
                    'attempt', ARGV[3],
                    'lease_until', tostring(lease_until),
                    'recovery_count', '0')
                redis.call('PEXPIRE', KEYS[1], ARGV[5])
                return {1, lease_until, 0, '', 0, ''}
            end

            if redis.call('HGET', KEYS[1], 'execution_key') ~= ARGV[2]
                or redis.call('HGET', KEYS[1], 'attempt') ~= ARGV[3] then
                return {4, 0, 0, '', 0, 'execution identity does not match record'}
            end

            local state = redis.call('HGET', KEYS[1], 'state')
            if state == 'completed' or state == 'retry_published' or state == 'dlq_published' then
                local completed_at = tonumber(redis.call('HGET', KEYS[1], 'completed_at') or '0')
                return {2, 0, 0, state, completed_at, ''}
            end
            if state ~= 'running' then
                return {4, 0, 0, '', 0, 'execution state is unknown'}
            end

            local current_until = tonumber(redis.call('HGET', KEYS[1], 'lease_until') or '0')
            if current_until <= now_ms then
                redis.call('HSET', KEYS[1],
                    'owner', ARGV[1],
                    'lease_until', tostring(lease_until),
                    'recovered_at', tostring(now_ms))
                redis.call('HINCRBY', KEYS[1], 'recovery_count', 1)
                redis.call('PEXPIRE', KEYS[1], ARGV[5])
                return {1, lease_until, 1, '', 0, ''}
            end

            return {3, current_until - now_ms, 0, '', 0, ''}
            "#,
        );

        let (code, lease_or_retry, recovered, terminal_state, completed_at, message): (
            i64,
            i64,
            i64,
            String,
            i64,
            String,
        ) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(key)
                .arg(&owner_token)
                .arg(&claim.execution_key)
                .arg(claim.attempt)
                .arg(lease_ttl_ms)
                .arg(running_retention_ms)
                .invoke_async(&mut connection),
        )
        .await?;

        match code {
            1 => Ok(ClaimOutcome::Acquired(ExecutionLease {
                execution_key: claim.execution_key.clone(),
                attempt: claim.attempt,
                owner_token,
                expires_at_unix_ms: u128::from(as_u64(
                    OPERATION,
                    "execution lease expiry",
                    lease_or_retry,
                )?),
                ttl: self.lease_ttl,
                recovered: recovered == 1,
            })),
            2 => {
                let outcome = terminal_state
                    .parse::<TerminalOutcome>()
                    .map_err(|error| CoordinationError::invalid_state(OPERATION, error))?;
                Ok(ClaimOutcome::AlreadyCompleted(CompletedOutcome {
                    outcome,
                    completed_at_unix_ms: u128::from(as_u64(
                        OPERATION,
                        "completion timestamp",
                        completed_at,
                    )?),
                }))
            }
            3 => Ok(ClaimOutcome::Busy {
                retry_after: duration_from_nonnegative_ms(lease_or_retry),
            }),
            4 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn renew(&self, lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError> {
        const OPERATION: &str = "execution_renew";
        let lease_ttl_ms = duration_ms(OPERATION, self.lease_ttl)?;
        let running_retention_ms = duration_ms(OPERATION, self.running_retention())?;
        let key = self.key_for_lease(lease);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(
            r#"
            if redis.call('EXISTS', KEYS[1]) == 0 then
                return {2, 0, 'execution record is absent'}
            end
            if redis.call('HGET', KEYS[1], 'execution_key') ~= ARGV[2]
                or redis.call('HGET', KEYS[1], 'attempt') ~= ARGV[3] then
                return {3, 0, 'execution identity does not match record'}
            end
            if redis.call('HGET', KEYS[1], 'state') ~= 'running' then
                return {3, 0, 'execution is no longer running'}
            end
            if redis.call('HGET', KEYS[1], 'owner') ~= ARGV[1] then
                return {2, 0, 'execution owner changed'}
            end

            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            local current_until = tonumber(redis.call('HGET', KEYS[1], 'lease_until') or '0')
            if current_until <= now_ms then
                return {2, 0, 'execution lease expired'}
            end
            local lease_until = now_ms + tonumber(ARGV[4])
            redis.call('HSET', KEYS[1], 'lease_until', tostring(lease_until))
            redis.call('PEXPIRE', KEYS[1], ARGV[5])
            return {1, lease_until, ''}
            "#,
        );
        let (code, expires_at, message): (i64, i64, String) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(key)
                .arg(&lease.owner_token)
                .arg(&lease.execution_key)
                .arg(lease.attempt)
                .arg(lease_ttl_ms)
                .arg(running_retention_ms)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            1 => {
                let mut renewed = lease.clone();
                renewed.expires_at_unix_ms =
                    u128::from(as_u64(OPERATION, "execution lease expiry", expires_at)?);
                renewed.ttl = self.lease_ttl;
                Ok(renewed)
            }
            2 => Err(CoordinationError::stale_owner(OPERATION)),
            3 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn complete(
        &self,
        lease: &ExecutionLease,
        outcome: TerminalOutcome,
    ) -> Result<CompletedOutcome, CoordinationError> {
        const OPERATION: &str = "execution_complete";
        let terminal_ttl_ms = duration_ms(OPERATION, self.terminal_ttl)?;
        let key = self.key_for_lease(lease);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(
            r#"
            if redis.call('EXISTS', KEYS[1]) == 0 then
                return {2, 0, 'execution record is absent'}
            end
            if redis.call('HGET', KEYS[1], 'execution_key') ~= ARGV[2]
                or redis.call('HGET', KEYS[1], 'attempt') ~= ARGV[3] then
                return {3, 0, 'execution identity does not match record'}
            end
            if redis.call('HGET', KEYS[1], 'owner') ~= ARGV[1] then
                return {2, 0, 'execution owner changed'}
            end

            local state = redis.call('HGET', KEYS[1], 'state')
            if state == 'completed' or state == 'retry_published' or state == 'dlq_published' then
                if state ~= ARGV[4] then
                    return {3, 0, 'execution already has a different terminal outcome'}
                end
                return {1, tonumber(redis.call('HGET', KEYS[1], 'completed_at') or '0'), ''}
            end
            if state ~= 'running' then
                return {3, 0, 'execution state is unknown'}
            end

            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            local lease_until = tonumber(redis.call('HGET', KEYS[1], 'lease_until') or '0')
            if lease_until <= now_ms then
                return {2, 0, 'execution lease expired'}
            end

            redis.call('HSET', KEYS[1],
                'state', ARGV[4],
                'completed_at', tostring(now_ms))
            redis.call('HDEL', KEYS[1], 'lease_until')
            redis.call('PEXPIRE', KEYS[1], ARGV[5])
            return {1, now_ms, ''}
            "#,
        );
        let (code, completed_at, message): (i64, i64, String) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(key)
                .arg(&lease.owner_token)
                .arg(&lease.execution_key)
                .arg(lease.attempt)
                .arg(outcome.as_str())
                .arg(terminal_ttl_ms)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            1 => Ok(CompletedOutcome {
                outcome,
                completed_at_unix_ms: u128::from(as_u64(
                    OPERATION,
                    "completion timestamp",
                    completed_at,
                )?),
            }),
            2 => Err(CoordinationError::stale_owner(OPERATION)),
            3 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }

    async fn release(&self, lease: &ExecutionLease) -> Result<ReleaseOutcome, CoordinationError> {
        const OPERATION: &str = "execution_release";
        let key = self.key_for_lease(lease);
        let mut connection = redis_operation(
            OPERATION,
            self.operation_timeout,
            self.client.get_multiplexed_tokio_connection(),
        )
        .await?;
        let script = Script::new(
            r#"
            if redis.call('EXISTS', KEYS[1]) == 0 then
                return {0, ''}
            end
            if redis.call('HGET', KEYS[1], 'execution_key') ~= ARGV[2]
                or redis.call('HGET', KEYS[1], 'attempt') ~= ARGV[3] then
                return {3, 'execution identity does not match record'}
            end
            if redis.call('HGET', KEYS[1], 'state') ~= 'running' then
                return {3, 'execution is no longer running'}
            end
            if redis.call('HGET', KEYS[1], 'owner') ~= ARGV[1] then
                return {2, 'execution owner changed'}
            end

            local now_parts = redis.call('TIME')
            local now_ms = tonumber(now_parts[1]) * 1000 + math.floor(tonumber(now_parts[2]) / 1000)
            if tonumber(redis.call('HGET', KEYS[1], 'lease_until') or '0') <= now_ms then
                return {2, 'execution lease expired'}
            end
            redis.call('DEL', KEYS[1])
            return {1, ''}
            "#,
        );
        let (code, message): (i64, String) = redis_operation(
            OPERATION,
            self.operation_timeout,
            script
                .key(key)
                .arg(&lease.owner_token)
                .arg(&lease.execution_key)
                .arg(lease.attempt)
                .invoke_async(&mut connection),
        )
        .await?;
        match code {
            0 => Ok(ReleaseOutcome::AlreadyAbsent),
            1 => Ok(ReleaseOutcome::Released),
            2 => Err(CoordinationError::stale_owner(OPERATION)),
            3 => Err(CoordinationError::invalid_state(OPERATION, message)),
            other => Err(CoordinationError::invalid_state(
                OPERATION,
                format!("unknown Redis response code {other}"),
            )),
        }
    }
}

pub type SharedRedisLeaderElector = Arc<RedisLeaderElector>;
pub type RedisDispatchStore = RedisDueStateStore;
pub type SharedRedisDueStateStore = Arc<RedisDueStateStore>;
pub type RedisExecutionLeaseStore = RedisIdempotencyStore;
pub type SharedRedisIdempotencyStore = Arc<RedisIdempotencyStore>;
