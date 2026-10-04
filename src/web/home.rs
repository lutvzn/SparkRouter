//! Compact gateway usage overview.

use askama::Template;
use axum::{extract::State, response::Html};

use super::render;
use crate::{
    app::AppState,
    db::usage::{self, Overview, ProviderTotals},
    error::AppError,
};

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    app_name: &'static str,
    requests: String,
    successes: String,
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
    let overview = usage::overview(&state.db).await?;
    render(build_home(overview))
}

fn build_home(data: Overview) -> Home {
    let total = data.requests;
    let requests = number(total);
    Home {
        app_name: crate::APP_NAME,
        requests,
        successes: number(data.successes),
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
    }
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

fn number(value: i64) -> String {
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

fn optional_cost(value: Option<f64>) -> String {
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
        });
        let html = populated.render().expect("render overview");
        assert!(!html.contains("<provider>"));
        assert!(html.contains("Reported for 0 of 1 requests"));
    }
}
