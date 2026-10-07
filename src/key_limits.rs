//! Process-local admission accounting. No lock spans database or upstream I/O.
use crate::{db::api_keys::KeyLimits, error::AppError};
use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Limiter {
    entries: Mutex<HashMap<String, Entry>>,
}
struct Entry {
    started: Instant,
    admitted: u32,
    active: u32,
    rejected: u64,
}
#[derive(Default)]
pub(crate) struct Snapshot {
    pub active: u32,
    pub rejected: u64,
}
pub(crate) struct Permit {
    limiter: Arc<Limiter>,
    id: String,
}
impl Limiter {
    pub fn admit(self: &Arc<Self>, id: &str, limits: KeyLimits) -> Result<Permit, AppError> {
        self.admit_at(id, limits, Instant::now())
    }
    fn admit_at(
        self: &Arc<Self>,
        id: &str,
        limits: KeyLimits,
        now: Instant,
    ) -> Result<Permit, AppError> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .retain(|_, e| e.active > 0 || now.duration_since(e.started) < Duration::from_secs(60));
        let entry = entries.entry(id.to_owned()).or_insert(Entry {
            started: now,
            admitted: 0,
            active: 0,
            rejected: 0,
        });
        let elapsed = now.duration_since(entry.started);
        if elapsed >= Duration::from_secs(60) {
            entry.started = now;
            entry.admitted = 0;
            entry.rejected = 0;
        }
        let rate_limited = limits
            .requests_per_minute
            .is_some_and(|max| entry.admitted >= max);
        let busy = limits.max_concurrent.is_some_and(|max| entry.active >= max);
        if rate_limited || busy {
            entry.rejected = entry.rejected.saturating_add(1);
            return Err(AppError::RateLimited(if rate_limited {
                60 - now.duration_since(entry.started).as_secs()
            } else {
                1
            }));
        }
        entry.admitted = entry.admitted.saturating_add(1);
        entry.active = entry.active.saturating_add(1);
        Ok(Permit {
            limiter: self.clone(),
            id: id.to_owned(),
        })
    }
    pub fn snapshot(&self, id: &str) -> Snapshot {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(id)
            .map(|e| Snapshot {
                active: e.active,
                rejected: if e.started.elapsed() < Duration::from_secs(60) {
                    e.rejected
                } else {
                    0
                },
            })
            .unwrap_or_default()
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut entries = self
            .limiter
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get_mut(&self.id) {
            entry.active = entry.active.saturating_sub(1);
        }
    }
}
impl Permit {
    /// Own the permit even before the first poll; dropping an unread body releases it.
    pub fn wrap(self, response: Response) -> Response {
        let (parts, body) = response.into_parts();
        let stream = async_stream::stream! {
            let mut permit = Some(self);
            let mut stream = body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                if chunk.is_err() { permit.take(); }
                yield chunk;
            }
            drop(permit);
        };
        Response::from_parts(parts, Body::from_stream(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_rejections_and_keys_are_independent() {
        let limiter = Arc::new(Limiter::default());
        let now = Instant::now();
        let limits = KeyLimits {
            requests_per_minute: Some(1),
            max_concurrent: None,
        };
        drop(limiter.admit_at("a", limits, now).unwrap());
        assert!(matches!(
            limiter.admit_at("a", limits, now + Duration::from_secs(10)),
            Err(AppError::RateLimited(50))
        ));
        drop(limiter.admit_at("b", limits, now).unwrap());
        drop(
            limiter
                .admit_at("a", limits, now + Duration::from_secs(60))
                .unwrap(),
        );
    }
    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        let limiter = Arc::new(Limiter::default());
        let barrier = Arc::new(std::sync::Barrier::new(32));
        let threads: Vec<_> = (0..32)
            .map(|_| {
                let limiter = limiter.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let permit = limiter
                        .admit(
                            "shared",
                            KeyLimits {
                                requests_per_minute: None,
                                max_concurrent: Some(3),
                            },
                        )
                        .ok();
                    barrier.wait();
                    permit
                })
            })
            .collect();
        let permits: Vec<_> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(permits.len(), 3);
        assert_eq!(limiter.snapshot("shared").active, 3);
        drop(permits);
        assert_eq!(limiter.snapshot("shared").active, 0);
    }

    #[tokio::test]
    async fn concurrent_admission_and_unpolled_body_drop_release_slots() {
        let limiter = Arc::new(Limiter::default());
        let limits = KeyLimits {
            requests_per_minute: None,
            max_concurrent: Some(1),
        };
        let permit = limiter.admit("a", limits).unwrap();
        assert!(matches!(
            limiter.admit("a", limits),
            Err(AppError::RateLimited(1))
        ));
        let response = permit.wrap(Response::new(Body::from("data")));
        assert_eq!(limiter.snapshot("a").active, 1);
        drop(response);
        assert_eq!(limiter.snapshot("a").active, 0);
        drop(limiter.admit("a", limits).unwrap());
    }
}
