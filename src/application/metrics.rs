use std::collections::HashMap;
use std::time::Duration;

use hdrhistogram::Histogram;

use crate::domain::contracts::{LATENCY_BUCKET_BOUNDS_MS, LatencyBucket};

pub struct MetricsBucket {
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub latency_ms: Histogram<u64>,
    latency_overflow: u64,
}

impl MetricsBucket {
    pub fn new() -> Self {
        Self {
            total: 0,
            success: 0,
            failure: 0,
            latency_ms: Histogram::new_with_bounds(1, 120_000, 3).expect("valid histogram"),
            latency_overflow: 0,
        }
    }

    pub fn record(&mut self, duration: Duration, ok: bool) {
        self.total += 1;
        if ok {
            self.success += 1;
        } else {
            self.failure += 1;
        }

        let ms = u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        if ms > 120_000 {
            self.latency_overflow = self.latency_overflow.saturating_add(1);
        } else {
            self.latency_ms
                .record(ms)
                .expect("latency within configured histogram bounds");
        }
    }

    pub fn merge_from(&mut self, other: &MetricsBucket) {
        self.total += other.total;
        self.success += other.success;
        self.failure += other.failure;
        let _ = self.latency_ms.add(&other.latency_ms);
        self.latency_overflow = self.latency_overflow.saturating_add(other.latency_overflow);
    }

    pub fn mergeable_latency_buckets(&self) -> Vec<LatencyBucket> {
        let mut lower_bound = 1_u64;
        let mut buckets = Vec::with_capacity(LATENCY_BUCKET_BOUNDS_MS.len() + 1);
        for upper_bound_ms in LATENCY_BUCKET_BOUNDS_MS {
            let count = if lower_bound > *upper_bound_ms {
                0
            } else {
                self.latency_ms.count_between(lower_bound, *upper_bound_ms)
            };
            buckets.push(LatencyBucket {
                upper_bound_ms: *upper_bound_ms,
                count,
            });
            lower_bound = upper_bound_ms.saturating_add(1);
        }
        buckets.push(LatencyBucket {
            upper_bound_ms: u64::MAX,
            count: self.latency_overflow,
        });
        buckets
    }
}

impl Default for MetricsBucket {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WorkerMetrics {
    pub scenario_metrics: HashMap<String, MetricsBucket>,
    pub step_metrics: HashMap<String, MetricsBucket>,
    pub error_counts: HashMap<String, u64>,
}

impl WorkerMetrics {
    pub fn new() -> Self {
        Self {
            scenario_metrics: HashMap::new(),
            step_metrics: HashMap::new(),
            error_counts: HashMap::new(),
        }
    }

    pub fn record_step(&mut self, step_name: &str, duration: Duration, ok: bool) {
        self.step_metrics
            .entry(step_name.to_string())
            .or_default()
            .record(duration, ok);
    }

    pub fn record_scenario(&mut self, scenario_name: &str, duration: Duration, ok: bool) {
        self.scenario_metrics
            .entry(scenario_name.to_string())
            .or_default()
            .record(duration, ok);
    }

    pub fn record_error_kind(&mut self, kind: String) {
        *self.error_counts.entry(kind).or_insert(0) += 1;
    }
}

impl Default for WorkerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

pub struct GlobalSummary {
    pub scenario_metrics: HashMap<String, MetricsBucket>,
    pub step_metrics: HashMap<String, MetricsBucket>,
    pub error_counts: HashMap<String, u64>,
}

impl GlobalSummary {
    pub fn new() -> Self {
        Self {
            scenario_metrics: HashMap::new(),
            step_metrics: HashMap::new(),
            error_counts: HashMap::new(),
        }
    }

    pub fn merge_worker(&mut self, worker: &WorkerMetrics) {
        for (name, bucket) in &worker.scenario_metrics {
            self.scenario_metrics
                .entry(name.clone())
                .or_default()
                .merge_from(bucket);
        }

        for (name, bucket) in &worker.step_metrics {
            self.step_metrics
                .entry(name.clone())
                .or_default()
                .merge_from(bucket);
        }

        for (kind, count) in &worker.error_counts {
            *self.error_counts.entry(kind.clone()).or_insert(0) += count;
        }
    }

    pub fn print_cli(&self, scenario_name: &str) {
        let scenario = self.scenario_metrics.get(scenario_name);
        if let Some(metrics) = scenario {
            println!("Scenario: {scenario_name}");
            println!("Total: {}", metrics.total);
            println!("Success: {}", metrics.success);
            println!("Failure: {}", metrics.failure);
            println!(
                "Scenario Latency p50/p95/p99: {}/{}/{} ms",
                metrics.latency_ms.value_at_quantile(0.50),
                metrics.latency_ms.value_at_quantile(0.95),
                metrics.latency_ms.value_at_quantile(0.99)
            );
        } else {
            println!("Scenario: {scenario_name}");
            println!("No scenario metrics collected.");
        }

        println!("Step Latency:");
        let mut step_names: Vec<_> = self.step_metrics.keys().cloned().collect();
        step_names.sort();
        for step in step_names {
            if let Some(metrics) = self.step_metrics.get(&step) {
                println!(
                    "  {step}: p50/p95/p99 {}/{}/{} ms | success {} failure {}",
                    metrics.latency_ms.value_at_quantile(0.50),
                    metrics.latency_ms.value_at_quantile(0.95),
                    metrics.latency_ms.value_at_quantile(0.99),
                    metrics.success,
                    metrics.failure
                );
            }
        }

        if !self.error_counts.is_empty() {
            println!("Error Breakdown:");
            let mut error_entries: Vec<_> = self.error_counts.iter().collect();
            error_entries.sort_by(|a, b| a.0.cmp(b.0));
            for (kind, count) in error_entries {
                println!("  {kind}: {count}");
            }
        }
    }
}

impl Default for GlobalSummary {
    fn default() -> Self {
        Self::new()
    }
}
