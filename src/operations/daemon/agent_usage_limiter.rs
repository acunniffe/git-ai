//! In-memory rate limiter for `agent_usage` telemetry events.
//!
//! The daemon's checkpoint side-effect path previously answered "should this
//! prompt emit an agent_usage event?" with a sqlite SELECT + upsert under the
//! global metrics-database mutex — per AI checkpoint. This limiter replaces
//! that disk transaction with a constant-time in-memory check. The
//! `agent_usage_throttle` table and its `MetricsDatabase` accessor are kept
//! for schema compatibility, but the checkpoint path no longer touches them.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const AGENT_USAGE_MIN_INTERVAL: Duration = Duration::from_secs(150);
const AGENT_USAGE_LIMITER_CAPACITY: usize = 10_000;

static AGENT_USAGE_LIMITER: OnceLock<Mutex<AgentUsageLimiter>> = OnceLock::new();

/// Returns whether an `agent_usage` event should be emitted for this
/// prompt_id, recording the emission time when it is allowed.
#[cfg_attr(any(test, feature = "test-support"), allow(dead_code))]
pub(crate) fn should_emit(prompt_id: &str) -> bool {
    let limiter = AGENT_USAGE_LIMITER.get_or_init(|| {
        Mutex::new(AgentUsageLimiter::new(
            AGENT_USAGE_MIN_INTERVAL,
            AGENT_USAGE_LIMITER_CAPACITY,
        ))
    });
    should_emit_with_limiter(limiter, prompt_id, Instant::now())
}

/// Throttling exists to reduce telemetry volume, so a poisoned lock fails
/// open: emitting an extra event beats silently dropping usage data.
fn should_emit_with_limiter(
    limiter: &Mutex<AgentUsageLimiter>,
    prompt_id: &str,
    now: Instant,
) -> bool {
    match limiter.lock() {
        Ok(mut limiter) => limiter.should_emit(prompt_id, now),
        Err(_) => true,
    }
}

/// Sliding-window suppressor with bounded memory: each prompt_id may emit at
/// most once per `interval`, and at most `capacity` prompt_ids are tracked.
/// `expiry_order` mirrors `last_emitted` insertion order so both expiry and
/// capacity eviction pop the oldest entry in O(1).
struct AgentUsageLimiter {
    interval: Duration,
    capacity: usize,
    last_emitted: HashMap<String, Instant>,
    expiry_order: VecDeque<(String, Instant)>,
}

impl AgentUsageLimiter {
    fn new(interval: Duration, capacity: usize) -> Self {
        Self {
            interval,
            capacity,
            last_emitted: HashMap::new(),
            expiry_order: VecDeque::new(),
        }
    }

    fn should_emit(&mut self, prompt_id: &str, now: Instant) -> bool {
        self.expire_stale(now);

        if self.last_emitted.contains_key(prompt_id) {
            return false;
        }

        while self.last_emitted.len() >= self.capacity {
            let Some((oldest_id, emitted_at)) = self.expiry_order.pop_front() else {
                break;
            };
            // A queue entry is only authoritative while it matches the map;
            // a mismatch means the id was re-tracked after this entry aged out.
            if self.last_emitted.get(&oldest_id) == Some(&emitted_at) {
                self.last_emitted.remove(&oldest_id);
            }
        }

        self.last_emitted.insert(prompt_id.to_string(), now);
        self.expiry_order.push_back((prompt_id.to_string(), now));
        true
    }

    fn expire_stale(&mut self, now: Instant) {
        while let Some((_, emitted_at)) = self.expiry_order.front() {
            if now.saturating_duration_since(*emitted_at) < self.interval {
                break;
            }
            let (prompt_id, emitted_at) = self
                .expiry_order
                .pop_front()
                .expect("front entry was just observed");
            if self.last_emitted.get(&prompt_id) == Some(&emitted_at) {
                self.last_emitted.remove(&prompt_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_until_the_interval_expires() {
        let mut limiter = AgentUsageLimiter::new(Duration::from_secs(100), 10);
        let t0 = Instant::now();

        assert!(limiter.should_emit("prompt-a", t0));
        assert!(!limiter.should_emit("prompt-a", t0 + Duration::from_secs(50)));
        assert!(!limiter.should_emit("prompt-a", t0 + Duration::from_secs(99)));
        assert!(limiter.should_emit("prompt-a", t0 + Duration::from_secs(100)));
    }

    #[test]
    fn capacity_is_bounded_and_evicts_the_oldest_valid_entry() {
        let mut limiter = AgentUsageLimiter::new(Duration::from_secs(1000), 2);
        let t0 = Instant::now();

        assert!(limiter.should_emit("prompt-a", t0));
        assert!(limiter.should_emit("prompt-b", t0 + Duration::from_secs(1)));
        assert!(limiter.should_emit("prompt-c", t0 + Duration::from_secs(2)));

        assert!(limiter.last_emitted.len() <= 2, "capacity must be enforced");
        // The oldest entry (prompt-a) was evicted, so it may emit again well
        // before its interval would have expired.
        assert!(limiter.should_emit("prompt-a", t0 + Duration::from_secs(3)));
        // The newest entries are still tracked and stay suppressed.
        assert!(!limiter.should_emit("prompt-c", t0 + Duration::from_secs(4)));
    }

    #[test]
    fn stale_entries_are_expired_before_capacity_eviction() {
        let mut limiter = AgentUsageLimiter::new(Duration::from_secs(10), 2);
        let t0 = Instant::now();

        assert!(limiter.should_emit("prompt-a", t0));
        assert!(limiter.should_emit("prompt-b", t0));

        // Both tracked entries are stale by t0+10s: a new prompt must expire
        // them rather than evicting still-valid state, and the expired
        // prompts may emit again.
        let t1 = t0 + Duration::from_secs(10);
        assert!(limiter.should_emit("prompt-c", t1));
        assert!(limiter.should_emit("prompt-a", t1));
        assert!(limiter.last_emitted.len() <= 2, "capacity must be enforced");
    }

    #[test]
    fn poisoned_lock_fails_open() {
        let limiter = std::sync::Arc::new(Mutex::new(AgentUsageLimiter::new(
            Duration::from_secs(100),
            10,
        )));
        let poisoner = limiter.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner
                .lock()
                .expect("lock must be healthy before poisoning");
            panic!("intentionally poison the limiter lock");
        })
        .join();

        assert!(
            should_emit_with_limiter(&limiter, "prompt-a", Instant::now()),
            "a poisoned limiter lock must fail open and allow the event"
        );
    }
}
