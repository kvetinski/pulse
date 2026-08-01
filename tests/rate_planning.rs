use std::time::Duration;

use pulse::application::service::planned_slice_loads;
use pulse::domain::contracts::PartitionKeyStrategy;
use pulse::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig};

fn scenario(rate: f64, concurrency: usize, duration: Duration) -> Scenario {
    Scenario::new(
        "rate-plan",
        Vec::new(),
        ScenarioConfig {
            endpoint: "http://127.0.0.1:50051".to_string(),
            scenarios_per_sec: rate,
            max_concurrency: concurrency,
            duration,
            repeat: RepeatPolicy::Once,
            partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
        },
    )
}

#[test]
fn slice_plans_conserve_global_rate_concurrency_and_explicit_burst() {
    let cases = [
        (0.1, 1, Duration::from_secs(10)),
        (9.9, 2, Duration::from_secs(1)),
        (20.0, 3, Duration::from_millis(100)),
        (200.5, 73, Duration::from_secs(1)),
        (1_000.0, 256, Duration::from_secs(1)),
    ];

    for (rate, concurrency, duration) in cases {
        let scenario = scenario(rate, concurrency, duration);
        let loads = planned_slice_loads(&scenario, 0);
        assert!(!loads.is_empty());
        let total_rate: f64 = loads.iter().map(|load| load.scenarios_per_sec).sum();
        let total_concurrency: usize = loads.iter().map(|load| load.max_concurrency).sum();
        assert!(
            (total_rate - rate).abs() <= 1e-9,
            "slice rates changed the global rate: configured={rate} planned={total_rate}"
        );
        assert_eq!(total_concurrency, concurrency);
        assert!(loads.iter().all(|load| load.max_concurrency > 0));

        for global_burst in 0..=concurrency {
            let burst_loads = planned_slice_loads(&scenario, global_burst);
            let bursts: Vec<_> = burst_loads.iter().map(|load| load.startup_burst).collect();
            assert_eq!(bursts.iter().sum::<usize>(), global_burst);
            assert!(
                bursts
                    .iter()
                    .zip(&burst_loads)
                    .all(|(burst, load)| *burst <= load.max_concurrency),
                "slice burst exceeded slice concurrency: burst={global_burst} loads={loads:?} bursts={bursts:?}"
            );
        }
    }
}
