//! Process-local admission accounting. No lock spans database or upstream I/O.
use crate::{
    db::api_keys::{KeyBudget, KeyLimits},
    error::AppError,
};
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

/// Usage counted against each budgeted key's current period. Loaded from
/// SQLite at the key's first request in a period, then kept here and added to
/// as requests finish; writes go through the history writer. Process-local
/// like the counters above: concurrent requests are admitted against what had
/// finished when they started, so a budget can be overrun by the requests
/// already in flight.
#[derive(Default)]
pub(crate) struct Spending {
    entries: Mutex<HashMap<String, Spent>>,
}

#[derive(Clone)]
struct Spent {
    period: String,
    tokens: i64,
    cost_usd: f64,
}

/// Charges a request's reported usage to the period it was admitted in.
#[derive(Clone)]
pub(crate) struct Charge {
    spending: Arc<Spending>,
    key: String,
    period: String,
}

impl Spending {
    pub async fn admit(
        self: &Arc<Self>,
        pool: &sqlx::SqlitePool,
        key: &str,
        budget: KeyBudget,
        now: i64,
    ) -> Result<Charge, AppError> {
        let period = budget.period.label(now);
        let cached = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .filter(|spent| spent.period == period)
            .cloned();
        let spent = match cached {
            Some(spent) => spent,
            None => {
                let (tokens, cost_usd) = crate::db::api_keys::spent(pool, key, &period).await?;
                let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
                // A concurrent first request may have loaded this period already.
                if entries.get(key).is_none_or(|spent| spent.period != period) {
                    let spent = Spent {
                        period: period.clone(),
                        tokens,
                        cost_usd,
                    };
                    entries.insert(key.to_owned(), spent);
                }
                entries[key].clone()
            }
        };
        if budget.tokens.is_some_and(|max| spent.tokens >= max)
            || budget.cost_usd.is_some_and(|max| spent.cost_usd >= max)
        {
            return Err(AppError::BudgetExceeded(budget.period.remaining(now)));
        }
        Ok(Charge {
            spending: self.clone(),
            key: key.to_owned(),
            period,
        })
    }
}

impl Charge {
    /// Count usage the upstream reported; unknown usage is not charged.
    pub fn add(
        self,
        writer: &crate::db::history_writer::HistoryWriter,
        tokens: Option<i64>,
        cost_usd: Option<f64>,
    ) {
        if tokens.is_none() && cost_usd.is_none() {
            return;
        }
        let tokens = tokens.unwrap_or(0);
        let cost_usd = cost_usd.unwrap_or(0.0);
        if let Some(spent) = self
            .spending
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.key)
            .filter(|spent| spent.period == self.period)
        {
            spent.tokens = spent.tokens.saturating_add(tokens);
            spent.cost_usd += cost_usd;
        }
        writer.spend(self.key, self.period, tokens, cost_usd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_periods_label_utc_days_and_months_and_count_down_to_the_next() {
        use crate::db::api_keys::BudgetPeriod;
        // 2026-10-21T13:00:00Z
        let now = 1_792_587_600;
        assert_eq!(BudgetPeriod::Day.label(now), "2026-10-21");
        assert_eq!(BudgetPeriod::Month.label(now), "2026-10");
        assert_eq!(BudgetPeriod::Day.remaining(now), 11 * 3600);
        assert_eq!(BudgetPeriod::Month.remaining(now), 10 * 86_400 + 11 * 3600);
        // Year and leap-day boundaries.
        assert_eq!(BudgetPeriod::Day.label(951_782_400), "2000-02-29");
        assert_eq!(BudgetPeriod::Month.remaining(1_798_761_599), 1);
        assert_eq!(BudgetPeriod::Month.label(1_798_761_600), "2027-01");
        assert_eq!(BudgetPeriod::Day.label(0), "1970-01-01");
    }

    #[tokio::test]
    async fn spending_refuses_at_the_limit_and_resets_with_the_period() {
        use crate::db::api_keys::{BudgetPeriod, KeyBudget};
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let writer = crate::db::history_writer::HistoryWriter::start(pool.clone());
        let spending = Arc::new(Spending::default());
        let budget = KeyBudget {
            period: BudgetPeriod::Day,
            tokens: Some(100),
            cost_usd: Some(1.0),
        };
        let now = 1_792_587_600;
        let admit = |now| spending.admit(&pool, "key", budget, now);
        admit(now).await.unwrap().add(&writer, Some(60), None);
        admit(now).await.unwrap().add(&writer, None, None);
        // Admitted today, finishing after midnight.
        let late = admit(now).await.unwrap();
        admit(now).await.unwrap().add(&writer, Some(40), Some(0.1));
        assert!(matches!(
            spending.admit(&pool, "key", budget, now).await,
            Err(AppError::BudgetExceeded(seconds)) if seconds == 11 * 3600
        ));
        // Another key and the next day start from nothing.
        assert!(spending.admit(&pool, "other", budget, now).await.is_ok());
        let tomorrow = spending
            .admit(&pool, "key", budget, now + 86_400)
            .await
            .unwrap();
        tomorrow.add(&writer, None, Some(1.0));
        assert!(matches!(
            spending.admit(&pool, "key", budget, now + 86_400).await,
            Err(AppError::BudgetExceeded(_))
        ));
        // A late completion from yesterday cannot reopen today.
        late.add(&writer, Some(1), None);
        assert!(
            spending
                .admit(&pool, "key", budget, now + 86_400)
                .await
                .is_err()
        );
    }

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
