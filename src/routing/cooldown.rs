//! Passive cooldown for targets that just failed, enabled in Settings.
//!
//! A retryable failure puts that exact connection/model in cooldown: the
//! upstream's `Retry-After` when it sent one, otherwise 5 seconds doubling with
//! each consecutive failure, up to 5 minutes. Cooling targets are not skipped.
//! They move to the end of a request's order, after every ready target, so a
//! request still tries each eligible target at most once and never fails only
//! because everything is cooling. A success ends the cooldown. Process-local,
//! bounded, and reset on restart; nothing runs in the background.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::providers::AttemptError;

const BASE: Duration = Duration::from_secs(5);
const MAX: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 4096;

struct Entry {
    failures: u32,
    until: Instant,
}

/// Entries by connection, then model, so lookups borrow `&str` keys.
#[derive(Default)]
struct Entries {
    targets: HashMap<String, HashMap<String, Entry>>,
    count: usize,
}

#[derive(Default)]
pub(crate) struct Cooldown {
    entries: Mutex<Entries>,
}

/// Move the items `cooling` selects after the others, keeping both groups in
/// order. Returns how many were moved.
pub(crate) fn defer<T>(items: &mut Vec<T>, cooling: impl Fn(&T) -> bool) -> usize {
    if !items.iter().any(&cooling) {
        return 0;
    }
    let (ready, deferred): (Vec<T>, Vec<T>) = items.drain(..).partition(|item| !cooling(item));
    let moved = deferred.len();
    items.extend(ready);
    items.extend(deferred);
    moved
}

/// Move the first item `preferred` selects to the front; the rest keep their
/// order.
pub(crate) fn prefer<T>(items: &mut [T], preferred: impl Fn(&T) -> bool) -> bool {
    match items.iter().position(preferred) {
        Some(index) => {
            items[..=index].rotate_right(1);
            true
        }
        None => false,
    }
}

impl Cooldown {
    pub(crate) fn is_cooling(&self, connection: &str, model: &str) -> bool {
        self.is_cooling_at(connection, model, Instant::now())
    }

    fn is_cooling_at(&self, connection: &str, model: &str, now: Instant) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        entries
            .targets
            .get(connection)
            .and_then(|models| models.get(model))
            .is_some_and(|entry| entry.until > now)
    }

    /// Record an attempt's result: a success ends the target's cooldown, a
    /// retryable failure starts or extends it, and a permanent error (a
    /// property of the request) changes nothing.
    pub(crate) fn record<T>(
        &self,
        connection: &str,
        model: &str,
        result: &Result<T, AttemptError>,
    ) {
        match result {
            Ok(_) => self.succeeded(connection, model),
            Err(AttemptError::Permanent(_)) => {}
            Err(error) => self.failed_at(connection, model, error.retry_after(), Instant::now()),
        }
    }

    fn failed_at(&self, connection: &str, model: &str, retry_after: Option<u64>, now: Instant) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let known = entries
            .targets
            .get(connection)
            .is_some_and(|models| models.contains_key(model));
        if !known && entries.count >= MAX_ENTRIES {
            for models in entries.targets.values_mut() {
                models.retain(|_, entry| entry.until > now);
            }
            entries.targets.retain(|_, models| !models.is_empty());
            entries.count = entries.targets.values().map(HashMap::len).sum();
            if entries.count >= MAX_ENTRIES {
                return;
            }
        }
        if !known {
            entries.count += 1;
        }
        let entry = entries
            .targets
            .entry(connection.to_owned())
            .or_default()
            .entry(model.to_owned())
            .or_insert(Entry {
                failures: 0,
                until: now,
            });
        entry.failures = entry.failures.saturating_add(1);
        let backoff = BASE.saturating_mul(1 << (entry.failures - 1).min(6));
        entry.until = now + retry_after.map_or(backoff, Duration::from_secs).min(MAX);
    }

    /// A success ends that target's cooldown and its failure count.
    fn succeeded(&self, connection: &str, model: &str) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let Some(models) = entries.targets.get_mut(connection) else {
            return;
        };
        if models.remove(model).is_some() {
            if models.is_empty() {
                entries.targets.remove(connection);
            }
            entries.count -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(cooldown: &Cooldown, now: Instant) -> (Vec<&'static str>, usize) {
        let mut items = vec!["a", "b", "c"];
        let moved = defer(&mut items, |item| cooldown.is_cooling_at(item, "m", now));
        (items, moved)
    }

    #[test]
    fn cooling_targets_move_last_in_order_until_they_expire() {
        let cooldown = Cooldown::default();
        let now = Instant::now();
        assert_eq!(order(&cooldown, now), (vec!["a", "b", "c"], 0));
        cooldown.failed_at("b", "m", None, now);
        cooldown.failed_at("a", "m", None, now);
        assert_eq!(order(&cooldown, now), (vec!["c", "a", "b"], 2));
        // Another model on the same connection is a different target.
        assert!(!cooldown.is_cooling_at("a", "x", now));
        assert_eq!(
            order(&cooldown, now + Duration::from_secs(5)),
            (vec!["a", "b", "c"], 0)
        );
    }

    #[test]
    fn backoff_doubles_honours_retry_after_and_resets_on_success() {
        let cooldown = Cooldown::default();
        let now = Instant::now();
        let until = |cooldown: &Cooldown| cooldown.entries.lock().unwrap().targets["a"]["m"].until;
        for expected in [5, 10, 20, 40, 80, 160, 300, 300] {
            cooldown.failed_at("a", "m", None, now);
            assert_eq!(until(&cooldown) - now, Duration::from_secs(expected));
        }
        cooldown.failed_at("a", "m", Some(42), now);
        assert_eq!(until(&cooldown) - now, Duration::from_secs(42));
        cooldown.failed_at("a", "m", Some(86_400), now);
        assert_eq!(until(&cooldown) - now, MAX);
        cooldown.succeeded("a", "m");
        cooldown.failed_at("a", "m", None, now);
        assert_eq!(until(&cooldown) - now, BASE);
    }

    #[test]
    fn results_start_end_or_leave_cooldowns_and_preferred_items_move_first() {
        let cooldown = Cooldown::default();
        let retryable: Result<(), _> = Err(AttemptError::RetryableStatus(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Some(30),
        ));
        cooldown.record("a", "m", &retryable);
        assert!(cooldown.is_cooling("a", "m"));
        let permanent: Result<(), _> = Err(AttemptError::Permanent(
            crate::error::AppError::Upstream(axum::http::StatusCode::BAD_REQUEST),
        ));
        cooldown.record("b", "m", &permanent);
        assert!(!cooldown.is_cooling("b", "m"));
        cooldown.record("a", "m", &Ok::<(), AttemptError>(()));
        assert!(!cooldown.is_cooling("a", "m"));
        assert_eq!(cooldown.entries.lock().unwrap().count, 0);

        let mut items = ["a", "b", "c", "d"];
        assert!(prefer(&mut items, |item| *item == "c"));
        assert_eq!(items, ["c", "a", "b", "d"]);
        assert!(!prefer(&mut items, |item| *item == "x"));
        assert_eq!(items, ["c", "a", "b", "d"]);
    }

    #[test]
    fn entries_are_bounded() {
        let cooldown = Cooldown::default();
        let now = Instant::now();
        for index in 0..MAX_ENTRIES + 10 {
            cooldown.failed_at(&index.to_string(), "m", None, now);
        }
        assert_eq!(cooldown.entries.lock().unwrap().count, MAX_ENTRIES);
        // Expired entries make room again.
        cooldown.failed_at("new", "m", None, now + MAX);
        assert_eq!(cooldown.entries.lock().unwrap().count, 1);
    }
}
