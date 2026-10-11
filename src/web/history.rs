//! Dashboard request history: metadata rows, aggregate stats, bounded opt-in
//! Extended body inspection, and a clear action with retention stated up front.

use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    response::{Html, Response},
};
use serde::Deserialize;

use super::{CsrfForm, check_csrf, render, see_other};
use crate::app::AppState;
use crate::db::request_history::{
    self, HistoryStats, OUTCOME_STREAMING, OUTCOME_SUCCESS, StoredRecord,
};
use crate::error::AppError;

/// Rows shown per history page.
const PAGE_SIZE: i64 = 50;

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryPage {
    app_name: &'static str,
    csrf_token: String,
    stats: HistoryStats,
    max_retained: i64,
    rows: Vec<HistoryRow>,
    page: i64,
    total_pages: i64,
}

/// One rendered table row. Display strings are prepared here so the template
/// only prints plain values; Askama still escapes every one of them.
struct HistoryRow {
    id: String,
    created_at: String,
    source: String,
    model: String,
    streaming: &'static str,
    outcome: String,
    outcome_class: &'static str,
    status: i64,
    latency_ms: i64,
    tokens: String,
    attempts: Vec<AttemptView>,
    routing: String,
}

/// One line inside a row's attempts disclosure; the surrounding ordered list
/// conveys the attempt position.
struct AttemptView {
    position: i64,
    provider_name: String,
    model: String,
    outcome: String,
    outcome_class: &'static str,
    latency_ms: i64,
    reason: String,
}

#[derive(Deserialize)]
pub(crate) struct HistoryQuery {
    page: Option<String>,
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Html<String>, AppError> {
    let total = request_history::count(&state.db).await?;
    let total_pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
    // Missing, non-numeric, or non-positive pages land on page one; pages
    // past the end are capped to the last page that still has rows.
    let requested = query
        .page
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);
    let page = requested.clamp(1, total_pages);
    let stored = request_history::list_page(&state.db, PAGE_SIZE, (page - 1) * PAGE_SIZE).await?;
    let stats = request_history::stats(&state.db).await?;
    let mut rows = Vec::with_capacity(stored.len());
    for record in stored {
        let diagnostics = request_history::diagnostics(&state.db, &record.record.id).await?;
        let mut row = HistoryRow::from_stored(record);
        row.routing = diagnostics.selection.map_or_else(
            || "Direct routing or historical record without selection metadata".into(),
            |selection| format!("Strategy: {} · strategy start: step {} · actual start: step {} · affinity: {} ({})",
                selection.strategy, selection.strategy_position, selection.starting_position,
                selection.affinity_mode, selection.affinity));
        for (attempt, reason) in row.attempts.iter_mut().zip(diagnostics.reasons) {
            attempt.reason = reason;
        }
        rows.push(row);
    }
    render(HistoryPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        stats,
        max_retained: crate::db::settings::history_retention(&state.db).await?,
        rows,
        page,
        total_pages,
    })
}

#[derive(Template)]
#[template(path = "history_payload.html")]
struct PayloadPage {
    app_name: &'static str,
    payload: Option<crate::db::request_payloads::Payload>,
}

pub(crate) async fn payload(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    use axum::response::IntoResponse;
    let page = render(PayloadPage {
        app_name: crate::APP_NAME,
        payload: crate::db::request_payloads::get(&state.db, &id).await?,
    })?;
    let mut response = page.into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

pub(crate) async fn clear(
    State(state): State<AppState>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    request_history::clear(&state.db).await?;
    Ok(see_other("/history"))
}

impl HistoryRow {
    fn from_stored(stored: StoredRecord) -> Self {
        let record = stored.record;
        Self {
            id: record.id,
            routing: String::new(),
            created_at: stored.created_at,
            source: record.source,
            model: record.model,
            streaming: if record.streaming { "yes" } else { "no" },
            outcome_class: outcome_class(&record.outcome),
            outcome: record.outcome,
            status: record.status,
            latency_ms: record.latency_ms,
            tokens: match (record.input_tokens, record.output_tokens) {
                (None, None) => "—".to_owned(),
                (input, output) => format!(
                    "{} / {}",
                    input.map_or_else(|| "—".into(), |n| n.to_string()),
                    output.map_or_else(|| "—".into(), |n| n.to_string())
                ),
            },
            attempts: record
                .attempts
                .into_iter()
                .map(|attempt| {
                    let outcome_class = outcome_class(&attempt.outcome);
                    AttemptView {
                        position: attempt.position,
                        provider_name: attempt.provider_name,
                        model: attempt.model,
                        outcome: attempt.outcome,
                        outcome_class,
                        latency_ms: attempt.latency_ms,
                        reason: "Reason unavailable for this historical record".into(),
                    }
                })
                .collect(),
        }
    }
}

/// Badge styling for an outcome label: success is green, pending (streaming)
/// stays neutral, anything else counts as a failure.
fn outcome_class(outcome: &str) -> &'static str {
    match outcome {
        OUTCOME_SUCCESS | "selected" => "badge ok",
        OUTCOME_STREAMING | "in_progress" => "badge",
        _ => "badge fail",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_usage_is_not_rendered_as_zero() {
        let row = HistoryRow::from_stored(StoredRecord {
            created_at: "now".into(),
            record: request_history::Record {
                id: "test".into(),
                source: "api".into(),
                model: "test".into(),
                streaming: false,
                outcome: "success".into(),
                status: 200,
                latency_ms: 1,
                input_tokens: Some(5),
                output_tokens: None,
                attempts: vec![],
            },
        });
        assert_eq!(row.tokens, "5 / —");
        assert_eq!(outcome_class("selected"), "badge ok");
    }
}
