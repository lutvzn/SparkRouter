//! Prometheus counters for `/metrics`, process-local and reset on restart.
//!
//! Counting starts once a scrape token exists (Settings or
//! `ROUTER_METRICS_TOKEN`); without one nothing is recorded and `/metrics`
//! answers 404. Labels stay low-cardinality and come only from configuration
//! the request actually reached: endpoint, combo name, provider slug, outcome.
//! A request for an unknown combo or model adds no label value. No key IDs,
//! model IDs from requests, prompts, or URLs are ever labels.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::app::AppState;

/// Upper bounds of the response-time histogram, in seconds.
const BUCKETS: [f64; 11] = [
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];
/// [`BUCKETS`] as `le` label values.
const BUCKET_LABELS: [&str; 11] = [
    "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "30", "60", "120", "300",
];

#[derive(Default)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

#[derive(Default)]
struct Registry {
    requests: BTreeMap<(&'static str, String, &'static str), u64>,
    durations: BTreeMap<&'static str, Histogram>,
    attempts: BTreeMap<(String, &'static str), u64>,
    tokens: BTreeMap<(String, &'static str), u64>,
    cost: BTreeMap<String, f64>,
    rejections: BTreeMap<&'static str, u64>,
    deferrals: u64,
    active_streams: u64,
}

pub(crate) struct Metrics {
    enabled: AtomicBool,
    started: f64,
    registry: Mutex<Registry>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |elapsed| elapsed.as_secs_f64()),
            registry: Mutex::default(),
        }
    }
}

/// One routed request's labels, recorded when its response is ready and,
/// for a stream, again when the body ends.
pub(crate) struct Request {
    pub endpoint: &'static str,
    /// The combo name, only when the combo existed.
    pub combo: String,
    /// The slug of the provider that served the request, if one did.
    pub provider: Option<String>,
}

impl Metrics {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// An admission rejection: `rate_limit`, `budget`, or `unauthorized`.
    pub(crate) fn rejected(&self, reason: &'static str) {
        if self.enabled() {
            *self.registry().rejections.entry(reason).or_default() += 1;
        }
    }

    /// Cooling targets a request moved to the end of its order.
    pub(crate) fn deferred(&self, count: usize) {
        if count > 0 && self.enabled() {
            self.registry().deferrals += count as u64;
        }
    }

    /// A response is ready: its time to headers and every attempt made.
    /// `streaming` responses count as requests only when their body ends.
    pub(crate) fn responded(
        &self,
        request: &Request,
        elapsed: Duration,
        attempts: &[(Option<String>, &'static str)],
        streaming: bool,
    ) {
        let mut registry = self.registry();
        let histogram = registry.durations.entry(request.endpoint).or_default();
        let seconds = elapsed.as_secs_f64();
        for (count, bound) in histogram.counts.iter_mut().zip(BUCKETS) {
            if seconds <= bound {
                *count += 1;
            }
        }
        histogram.count += 1;
        histogram.sum += seconds;
        for (provider, outcome) in attempts {
            if let Some(provider) = provider {
                *registry
                    .attempts
                    .entry((provider.clone(), outcome))
                    .or_default() += 1;
            }
        }
        if streaming {
            registry.active_streams += 1;
        }
    }

    /// A request finished: `success`, `failure`, `interrupted`, or `cancelled`.
    pub(crate) fn finished(
        &self,
        request: &Request,
        outcome: &'static str,
        usage: crate::history::Usage,
        streamed: bool,
    ) {
        let mut registry = self.registry();
        if streamed {
            registry.active_streams = registry.active_streams.saturating_sub(1);
        }
        *registry
            .requests
            .entry((request.endpoint, request.combo.clone(), outcome))
            .or_default() += 1;
        let Some(provider) = &request.provider else {
            return;
        };
        for (kind, count) in [
            ("input", usage.input),
            ("output", usage.output),
            ("cached", usage.cached),
        ] {
            if let Some(count) = count.and_then(|count| u64::try_from(count).ok()) {
                *registry.tokens.entry((provider.clone(), kind)).or_default() += count;
            }
        }
        if let Some(cost) = usage.cost_usd {
            *registry.cost.entry(provider.clone()).or_default() += cost;
        }
    }

    /// The text exposition format, version 0.0.4: each family's HELP and
    /// TYPE lines, then its samples.
    fn render(&self) -> String {
        let registry = self.registry();
        let mut out = String::new();
        single(
            &mut out,
            (
                "sparkrouter_build_info",
                "gauge",
                "Version of the running gateway.",
            ),
            &[("version", crate::APP_VERSION)],
            1,
        );
        single(
            &mut out,
            (
                "sparkrouter_process_start_time_seconds",
                "gauge",
                "Start of this process, in Unix seconds.",
            ),
            &[],
            format_args!("{:.3}", self.started),
        );
        let name = family(
            &mut out,
            "sparkrouter_requests_total",
            "counter",
            "Routed requests by endpoint, combo (empty for direct models), and outcome.",
        );
        for ((endpoint, combo, outcome), count) in &registry.requests {
            let labels = [
                ("endpoint", *endpoint),
                ("combo", combo),
                ("outcome", outcome),
            ];
            sample(&mut out, name, &labels, count);
        }
        family(
            &mut out,
            "sparkrouter_request_duration_seconds",
            "histogram",
            "Time until response headers were ready, fallback included.",
        );
        for (endpoint, histogram) in &registry.durations {
            let bucket = "sparkrouter_request_duration_seconds_bucket";
            for (count, bound) in histogram.counts.iter().zip(BUCKET_LABELS) {
                sample(
                    &mut out,
                    bucket,
                    &[("endpoint", endpoint), ("le", bound)],
                    count,
                );
            }
            let total = [("endpoint", *endpoint)];
            sample(
                &mut out,
                bucket,
                &[("endpoint", endpoint), ("le", "+Inf")],
                histogram.count,
            );
            sample(
                &mut out,
                "sparkrouter_request_duration_seconds_sum",
                &total,
                histogram.sum,
            );
            sample(
                &mut out,
                "sparkrouter_request_duration_seconds_count",
                &total,
                histogram.count,
            );
        }
        let name = family(
            &mut out,
            "sparkrouter_upstream_attempts_total",
            "counter",
            "Upstream attempts by provider slug and outcome.",
        );
        for ((provider, outcome), count) in &registry.attempts {
            sample(
                &mut out,
                name,
                &[("provider", provider), ("outcome", outcome)],
                count,
            );
        }
        let name = family(
            &mut out,
            "sparkrouter_tokens_total",
            "counter",
            "Tokens the serving upstream reported, by provider slug and type.",
        );
        for ((provider, kind), count) in &registry.tokens {
            sample(
                &mut out,
                name,
                &[("provider", provider), ("type", kind)],
                count,
            );
        }
        let name = family(
            &mut out,
            "sparkrouter_cost_usd_total",
            "counter",
            "USD cost the serving upstream reported, by provider slug.",
        );
        for (provider, cost) in &registry.cost {
            sample(&mut out, name, &[("provider", provider)], cost);
        }
        let name = family(
            &mut out,
            "sparkrouter_rejections_total",
            "counter",
            "Requests refused before routing, by reason.",
        );
        for (reason, count) in &registry.rejections {
            sample(&mut out, name, &[("reason", reason)], count);
        }
        single(
            &mut out,
            (
                "sparkrouter_cooldown_deferrals_total",
                "counter",
                "Cooling targets moved to the end of a request's order.",
            ),
            &[],
            registry.deferrals,
        );
        single(
            &mut out,
            (
                "sparkrouter_active_streams",
                "gauge",
                "Streaming responses whose body has not ended yet.",
            ),
            &[],
            registry.active_streams,
        );
        out
    }
}

/// Write a family's HELP and TYPE lines; returns its name for the samples.
fn family<'a>(out: &mut String, name: &'a str, kind: &str, help: &str) -> &'a str {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    name
}

/// A family with one sample.
fn single(
    out: &mut String,
    (name, kind, help): (&str, &str, &str),
    labels: &[(&str, &str)],
    value: impl std::fmt::Display,
) {
    let name = family(out, name, kind, help);
    sample(out, name, labels, value);
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (index, (key, label)) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(out, "{key}=\"{}\"", escape(label));
        }
        out.push('}');
    }
    let _ = writeln!(out, " {value}");
}

/// Label values need `\\`, `\"` and `\n` escaped; most have none.
fn escape(value: &str) -> std::borrow::Cow<'_, str> {
    if !value.contains(['\\', '"', '\n']) {
        return value.into();
    }
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .into()
}

/// The stored form of a metrics token: lowercase hex SHA-256. The token is
/// 256 random bits, so an unsalted fast hash is enough.
pub(crate) fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

pub fn router() -> Router<AppState> {
    Router::new().route("/metrics", get(scrape))
}

/// `GET /metrics` with `Authorization: Bearer <token>`. 404 while no token
/// is configured, 401 for a missing or wrong one.
async fn scrape(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let stored = match crate::db::settings::metrics_token_hash(&state.db).await {
        Ok(stored) => stored,
        Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "metrics unavailable\n").into_response();
        }
    };
    if stored.is_none() && state.metrics_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let supplied = crate::app::bearer_token(&headers);
    let environment = state
        .metrics_token
        .as_ref()
        .is_some_and(|expected| crate::app::secret_matches(supplied, expected));
    let dashboard = !supplied.is_empty()
        && stored.is_some_and(|stored| {
            bool::from(token_hash(supplied).as_bytes().ct_eq(stored.as_bytes()))
        });
    if !environment && !dashboard {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "metrics token required\n",
        )
            .into_response();
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(combo: &str) -> Request {
        Request {
            endpoint: "chat",
            combo: combo.into(),
            provider: Some("open\"ai".into()),
        }
    }

    #[test]
    fn exposition_lists_families_samples_and_escapes_labels() {
        let metrics = Metrics::default();
        metrics.set_enabled(true);
        let request = request("coding");
        metrics.responded(
            &request,
            Duration::from_millis(300),
            &[
                (Some("open\"ai".into()), "retryable_failure"),
                (Some("open\"ai".into()), "selected"),
                (None, "selected"),
            ],
            true,
        );
        metrics.deferred(1);
        metrics.finished(
            &request,
            "success",
            crate::history::Usage {
                input: Some(10),
                output: Some(4),
                cached: None,
                cost_usd: Some(0.25),
            },
            true,
        );
        metrics.rejected("budget");
        let text = metrics.render();
        assert!(text.contains("# TYPE sparkrouter_requests_total counter\n"));
        assert!(text.contains(
            "sparkrouter_requests_total{endpoint=\"chat\",combo=\"coding\",outcome=\"success\"} 1\n"
        ));
        assert!(text.contains(
            "sparkrouter_request_duration_seconds_bucket{endpoint=\"chat\",le=\"0.25\"} 0\n"
        ));
        assert!(text.contains(
            "sparkrouter_request_duration_seconds_bucket{endpoint=\"chat\",le=\"0.5\"} 1\n"
        ));
        assert!(text.contains(
            "sparkrouter_request_duration_seconds_bucket{endpoint=\"chat\",le=\"+Inf\"} 1\n"
        ));
        assert!(text.contains(
            "sparkrouter_upstream_attempts_total{provider=\"open\\\"ai\",outcome=\"selected\"} 1\n"
        ));
        assert!(
            text.contains("sparkrouter_tokens_total{provider=\"open\\\"ai\",type=\"input\"} 10\n")
        );
        assert!(!text.contains("type=\"cached\""));
        assert!(text.contains("sparkrouter_cost_usd_total{provider=\"open\\\"ai\"} 0.25\n"));
        assert!(text.contains("sparkrouter_rejections_total{reason=\"budget\"} 1\n"));
        assert!(text.contains("sparkrouter_cooldown_deferrals_total 1\n"));
        assert!(text.contains("sparkrouter_active_streams 0\n"));
    }

    #[test]
    fn rejections_are_ignored_until_enabled_and_hashes_are_hex() {
        let metrics = Metrics::default();
        metrics.rejected("rate_limit");
        assert!(!metrics.render().contains("rate_limit"));
        metrics.set_enabled(true);
        metrics.rejected("rate_limit");
        assert!(metrics.render().contains("reason=\"rate_limit\"} 1"));
        let hash = token_hash("token");
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(hash, token_hash("other"));
    }
}
