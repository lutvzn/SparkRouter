//! Compact gateway usage overview.

use askama::Template;
use axum::{extract::State, response::Html};

use super::render;
use crate::{
    app::AppState,
    db::{
        combos::AffinityMode,
        usage::{self, ComboCache, Overview, ProviderTotals},
    },
    error::AppError,
};

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    app_name: &'static str,
    successes: String,
    failures: String,
    database_status: &'static str,
    memory: String,
    usage_available: bool,
    input_tokens: String,
    output_tokens: String,
    cached_tokens: String,
    cost: String,
    input_coverage: String,
    output_coverage: String,
    cache_coverage: String,
    cost_coverage: String,
    has_data: bool,
    providers: Vec<ProviderView>,
    combos: Vec<ComboCacheView>,
}

struct ComboCacheView {
    name: String,
    affinity: String,
    requests: String,
    hit_rate: String,
    cached_share: String,
}

struct ProviderView {
    name: String,
    slug: String,
    requests: String,
    input_tokens: String,
    output_tokens: String,
    cached_tokens: String,
    cost: String,
    coverage: String,
    cost_coverage: String,
    proportion: i64,
}

pub(crate) async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let database_ready = crate::health::database_ready(&state.db).await;
    let overview = if database_ready {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            usage::overview(&state.db),
        )
        .await
        .ok()
        .and_then(Result::ok)
    } else {
        None
    };
    let mut home = match overview {
        Some(data) => build_home(data),
        None => {
            let mut home = build_home(Overview::default());
            home.usage_available = false;
            home.successes = "—".into();
            home.failures = "—".into();
            home.input_coverage = "Usage unavailable".into();
            home.output_coverage = "Usage unavailable".into();
            home.cache_coverage = "Usage unavailable".into();
            home.cost_coverage = "Usage unavailable".into();
            home
        }
    };
    home.database_status = if database_ready {
        "Connected"
    } else {
        "Unavailable"
    };
    home.memory = process_memory().await;
    render(home)
}

fn build_home(data: Overview) -> Home {
    let total = data.requests;
    Home {
        app_name: crate::APP_NAME,
        successes: number(data.successes),
        failures: number(total - data.successes),
        database_status: "Unavailable",
        memory: "Unavailable".into(),
        usage_available: true,
        input_tokens: optional_number(data.input_tokens),
        output_tokens: optional_number(data.output_tokens),
        cached_tokens: optional_number(data.cached_tokens),
        cost: optional_cost(data.cost_usd),
        input_coverage: coverage(data.input_known, total),
        output_coverage: coverage(data.output_known, total),
        cache_coverage: coverage(data.cache_known, total),
        cost_coverage: coverage(data.cost_known, total),
        has_data: total > 0,
        providers: data
            .providers
            .into_iter()
            .take(10)
            .map(|p| provider_view(p, total))
            .collect(),
        combos: data.combos.into_iter().map(combo_cache_view).collect(),
    }
}

fn percent(part: i64, whole: i64) -> i64 {
    (part * 100 / whole).clamp(0, 100)
}

fn combo_cache_view(c: ComboCache) -> ComboCacheView {
    let affinity = match c.affinity_mode.as_deref().map(AffinityMode::parse) {
        None => "Combo no longer exists".to_owned(),
        Some(Ok(AffinityMode::Off)) => "Cache-awareness off".to_owned(),
        Some(Ok(mode)) => format!(
            "{} · {} min",
            mode.label(),
            c.affinity_minutes.unwrap_or_default()
        ),
        Some(Err(_)) => "Unknown mode".to_owned(),
    };
    ComboCacheView {
        name: c.name,
        affinity,
        requests: number(c.requests),
        hit_rate: if c.reported > 0 {
            format!(
                "{}% · {} of {} reporting requests",
                percent(c.hits, c.reported),
                number(c.hits),
                number(c.reported)
            )
        } else {
            "Not reported".to_owned()
        },
        cached_share: match (c.cached_tokens, c.input_tokens) {
            (Some(cached), Some(input)) if input > 0 => {
                format!("{}% of input tokens", percent(cached, input))
            }
            _ => "—".to_owned(),
        },
    }
}

// Linux exposes current resident memory without inspecting other processes or
// adding a platform monitoring dependency. Unsupported platforms stay unknown.
pub(super) async fn process_memory() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(status) = tokio::fs::read_to_string("/proc/self/status").await
        && let Some(kib) = resident_kib(&status)
    {
        return format!("{:.1} MiB", kib as f64 / 1024.0);
    }
    "Unavailable".into()
}

#[cfg(any(target_os = "linux", test))]
fn resident_kib(status: &str) -> Option<u64> {
    let mut fields = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace();
    let kib = fields.next()?.parse().ok()?;
    (fields.next()? == "kB" && fields.next().is_none()).then_some(kib)
}

fn provider_view(p: ProviderTotals, total: i64) -> ProviderView {
    ProviderView {
        name: p.name,
        slug: p.slug,
        requests: number(p.requests),
        input_tokens: optional_number(p.input_tokens),
        output_tokens: optional_number(p.output_tokens),
        cached_tokens: optional_number(p.cached_tokens),
        cost: optional_cost(p.cost_usd),
        coverage: coverage(p.usage_requests, p.requests),
        cost_coverage: coverage(p.cost_known, p.requests),
        proportion: if total > 0 {
            (p.requests * 100 / total).clamp(0, 100)
        } else {
            0
        },
    }
}

pub(super) fn number(value: i64) -> String {
    let raw = value.to_string();
    let mut result = String::with_capacity(raw.len() + raw.len() / 3);
    for (i, ch) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

fn optional_number(value: Option<i64>) -> String {
    value.map_or_else(|| "—".into(), number)
}

pub(super) fn optional_cost(value: Option<f64>) -> String {
    match value {
        None => "—".into(),
        Some(n) if n != 0.0 && n.abs() < 0.0001 => "< $0.0001".into(),
        Some(n) => format!("${n:.4}"),
    }
}

fn coverage(known: i64, total: i64) -> String {
    format!(
        "Reported for {} of {} requests",
        number(known),
        number(total)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_resident_memory_without_guessing() {
        assert_eq!(resident_kib("Name: router\nVmRSS:\t2048 kB\n"), Some(2048));
        assert_eq!(resident_kib("VmRSS: 0 kB"), Some(0));
        for status in [
            "VmSize: 2048 kB",
            "VmRSS: unknown kB",
            "VmRSS: 2048 MB",
            "VmRSS: -1 kB",
        ] {
            assert_eq!(resident_kib(status), None);
        }
    }

    #[test]
    fn formats_counts_and_small_costs_without_losing_unknowns() {
        assert_eq!(number(1234567), "1,234,567");
        assert_eq!(optional_cost(Some(0.00001)), "< $0.0001");
        assert_eq!(optional_cost(None), "—");
    }

    #[test]
    fn renders_empty_and_escapes_provider_names() {
        let empty = build_home(Overview {
            requests: 0,
            successes: 0,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_usd: None,
            input_known: 0,
            output_known: 0,
            cache_known: 0,
            cost_known: 0,
            providers: vec![],
            combos: vec![],
        });
        let html = empty.render().expect("render empty overview");
        assert!(html.contains("No requests yet"));
        let populated = build_home(Overview {
            requests: 1,
            successes: 1,
            input_tokens: Some(2),
            output_tokens: Some(3),
            cached_tokens: None,
            cost_usd: None,
            input_known: 1,
            output_known: 1,
            cache_known: 0,
            cost_known: 0,
            providers: vec![ProviderTotals {
                slug: "<slug>".into(),
                name: "<provider>".into(),
                requests: 1,
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                cost_usd: None,
                usage_requests: 0,
                cost_known: 0,
            }],
            combos: vec![],
        });
        let html = populated.render().expect("render overview");
        assert!(!html.contains("<provider>"));
        assert!(html.contains("Reported for 0 of 1 requests"));
        assert!(!html.contains("id=\"cache-heading\""));
    }

    #[test]
    fn combo_cache_rates_count_only_reporting_requests() {
        let combo = |mode: Option<&str>, reported, hits, cached, input| ComboCache {
            name: "<coding>".into(),
            affinity_mode: mode.map(str::to_owned),
            affinity_minutes: mode.map(|_| 5),
            requests: 12,
            reported,
            hits,
            cached_tokens: cached,
            input_tokens: input,
        };
        let view = combo_cache_view(combo(Some("smart"), 8, 6, Some(3000), Some(4000)));
        assert_eq!(view.affinity, "Smart · 5 min");
        assert_eq!(view.hit_rate, "75% · 6 of 8 reporting requests");
        assert_eq!(view.cached_share, "75% of input tokens");
        let view = combo_cache_view(combo(Some("off"), 0, 0, None, None));
        assert_eq!(view.affinity, "Cache-awareness off");
        assert_eq!(view.hit_rate, "Not reported");
        assert_eq!(view.cached_share, "—");
        assert_eq!(
            combo_cache_view(combo(None, 1, 0, Some(0), Some(0))).affinity,
            "Combo no longer exists"
        );
        let html = build_home(Overview {
            requests: 12,
            combos: vec![combo(Some("basic"), 8, 6, Some(3000), Some(4000))],
            ..Overview::default()
        })
        .render()
        .expect("render combo cache");
        assert!(html.contains("id=\"cache-heading\""));
        assert!(html.contains("combo/&lt;coding&gt;") || html.contains("combo/&#60;coding&#62;"));
    }
}
