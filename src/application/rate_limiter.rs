use std::time::Duration;

use tokio::time::{Instant, sleep};

pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Creates a smoothly paced limiter.
    ///
    /// No start is available immediately. The first token arrives halfway
    /// through the first inter-arrival interval. This centered schedule gives
    /// a finite window its configured expected count without creating an
    /// implicit startup burst.
    pub fn new(scenarios_per_sec: f64) -> Self {
        Self::build(scenarios_per_sec, 1)
    }

    /// Creates a limiter that explicitly permits `burst_capacity` immediate
    /// starts and can accumulate that many tokens after an idle period.
    pub fn with_burst(scenarios_per_sec: f64, burst_capacity: usize) -> Self {
        assert!(
            burst_capacity > 0,
            "burst_capacity must be greater than zero"
        );
        Self::build_with_initial_tokens(scenarios_per_sec, burst_capacity)
    }

    fn build(scenarios_per_sec: f64, burst_capacity: usize) -> Self {
        assert!(
            scenarios_per_sec.is_finite() && scenarios_per_sec > 0.0,
            "scenarios_per_sec must be finite and greater than zero"
        );

        let capacity = burst_capacity as f64;
        Self {
            capacity,
            refill_per_sec: scenarios_per_sec,
            // Center the first arrival in its interval. This avoids an
            // immediate implicit burst while giving a window that spans one
            // full interval exactly one scheduled start.
            tokens: 0.5,
            last_refill: Instant::now(),
        }
    }

    fn build_with_initial_tokens(scenarios_per_sec: f64, burst_capacity: usize) -> Self {
        let mut bucket = Self::build(scenarios_per_sec, burst_capacity);
        bucket.tokens = bucket.capacity;
        bucket
    }

    pub async fn acquire(&mut self) {
        loop {
            self.refill();
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            let missing = 1.0 - self.tokens;
            let wait_secs = missing / self.refill_per_sec;
            // Tokio's clock has nanosecond resolution. Avoid a zero-duration
            // spin for rates above that resolution; runtime configuration
            // applies a substantially lower safety ceiling.
            let wait = Duration::from_secs_f64(wait_secs).max(Duration::from_nanos(1));
            sleep(wait).await;
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

    use super::TokenBucket;

    #[tokio::test(start_paused = true)]
    async fn supports_fractional_rates_without_a_one_sps_floor() {
        let mut bucket = TokenBucket::new(0.1);
        let started_at = Instant::now();

        bucket.acquire().await;
        assert_eq!(started_at.elapsed(), Duration::from_secs(5));

        bucket.acquire().await;
        assert_eq!(started_at.elapsed(), Duration::from_secs(15));

        bucket.acquire().await;
        assert_eq!(started_at.elapsed(), Duration::from_secs(25));
    }

    #[tokio::test(start_paused = true)]
    async fn default_limiter_does_not_release_multiple_startup_tokens() {
        let mut bucket = TokenBucket::new(100.0);
        let started_at = Instant::now();

        bucket.acquire().await;
        assert_eq!(started_at.elapsed(), Duration::from_millis(5));

        bucket.acquire().await;
        assert_eq!(started_at.elapsed(), Duration::from_millis(15));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_burst_constructor_releases_only_the_configured_burst() {
        let mut bucket = TokenBucket::with_burst(2.0, 3);
        let started_at = Instant::now();

        for _ in 0..3 {
            bucket.acquire().await;
        }
        assert_eq!(started_at.elapsed(), Duration::ZERO);

        bucket.acquire().await;
        assert_eq!(started_at.elapsed(), Duration::from_millis(500));
    }

    #[test]
    #[should_panic(expected = "scenarios_per_sec must be finite and greater than zero")]
    fn rejects_non_positive_rates() {
        let _ = TokenBucket::new(0.0);
    }

    #[test]
    #[should_panic(expected = "scenarios_per_sec must be finite and greater than zero")]
    fn rejects_non_finite_rates() {
        let _ = TokenBucket::new(f64::INFINITY);
    }
}
