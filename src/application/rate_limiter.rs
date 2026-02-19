use std::time::{Duration, Instant};

use tokio::time::sleep;

pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(scenarios_per_sec: f64) -> Self {
        let capacity = scenarios_per_sec.max(1.0);
        Self {
            capacity,
            refill_per_sec: scenarios_per_sec.max(1.0),
            tokens: capacity,
            last_refill: Instant::now(),
        }
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
            sleep(Duration::from_secs_f64(wait_secs)).await;
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }
}
