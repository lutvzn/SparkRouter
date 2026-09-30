//! Combo admin pages and actions.

use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use super::{CsrfForm, check_csrf, render, see_other};
use crate::app::AppState;
use crate::error::AppError;

const NAME_MAX_CHARS: usize = 64;
const MODEL_MAX_CHARS: usize = 256;

#[derive(Template)]
#[template(path = "combos/list.html")]
pub(crate) struct CombosPage {
    app_name: &'static str,
    csrf_token: String,
    combos: Vec<ComboRow>,
}

pub(crate) struct ComboRow {
    id: String,
    name: String,
    step_summary: String,
}

#[derive(Template)]
#[template(path = "combos/new.html")]
pub(crate) struct ComboNewPage {
    app_name: &'static str,
    csrf_token: String,
}

#[derive(Template)]
#[template(path = "combos/edit.html")]
pub(crate) struct ComboEditPage {
    app_name: &'static str,
    csrf_token: String,
    combo_id: String,
    combo_name: String,
    steps: Vec<StepRow>,
    providers: Vec<ProviderOption>,
}

/// HTMX swap target for step mutations; also included by the full edit page.
#[derive(Template)]
#[template(path = "combos/_steps.html")]
pub(crate) struct ComboStepsFragment {
    csrf_token: String,
    combo_id: String,
    steps: Vec<StepRow>,
}

pub(crate) struct StepRow {
    id: String,
    provider_name: String,
    /// True when the referenced provider is disabled and skipped by routing.
    provider_disabled: bool,
    model: String,
}

pub(crate) struct ProviderOption {
    id: String,
    name: String,
    /// Appended as "(disabled)" in the picker so skipped upstreams are visible.
    disabled: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ComboForm {
    _csrf: String,
    name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StepForm {
    _csrf: String,
    provider_id: String,
    model: String,
}

pub(crate) async fn list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let stored = crate::db::combos::list(&state.db).await?;
    let mut combos = Vec::with_capacity(stored.len());
    for combo in stored {
        let step_count = crate::db::combos::steps(&state.db, &combo.id).await?.len();
        combos.push(ComboRow {
            step_summary: match step_count {
                0 => "no steps".to_string(),
                1 => "1 step".to_string(),
                n => format!("{n} steps"),
            },
            id: combo.id,
            name: combo.name,
        });
    }
    render(CombosPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        combos,
    })
}

pub(crate) async fn new(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    render(ComboNewPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
    })
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Form(form): Form<ComboForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let name = validate_combo_name(&form.name)?;
    let combo = crate::db::combos::create(&state.db, &name).await?;
    Ok(see_other(&format!("/combos/{}", combo.id)))
}

pub(crate) async fn edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let combo = crate::db::combos::get(&state.db, &id).await?;
    let steps = step_rows(&state, &combo.id).await?;
    let providers = provider_options(&state).await?;
    render(ComboEditPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        combo_id: combo.id,
        combo_name: combo.name,
        steps,
        providers,
    })
}

pub(crate) async fn rename(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<ComboForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let name = validate_combo_name(&form.name)?;
    crate::db::combos::rename(&state.db, &id, &name).await?;
    Ok(see_other(&format!("/combos/{id}")))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::combos::delete(&state.db, &id).await?;
    Ok(see_other("/combos"))
}

/// Adds a step. HTMX requests (hx-request: true) get the re-rendered steps
/// fragment; plain form posts get redirected back to the combo editor so the
/// dashboard navigation stays intact without JavaScript.
pub(crate) async fn add_step(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<StepForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let provider_id = form.provider_id.trim();
    if provider_id.is_empty() {
        return Err(AppError::InvalidRequest("provider is required".into()));
    }
    let model = form.model.trim();
    if model.is_empty() {
        return Err(AppError::InvalidRequest("model is required".into()));
    }
    if model.chars().count() > MODEL_MAX_CHARS {
        return Err(AppError::InvalidRequest(format!(
            "model must be at most {MODEL_MAX_CHARS} characters"
        )));
    }
    crate::db::combos::add_step(&state.db, &id, provider_id, model).await?;
    step_mutation_response(&state, &headers, &id).await
}

pub(crate) async fn remove_step(
    State(state): State<AppState>,
    Path((id, step_id)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::combos::remove_step(&state.db, &id, &step_id).await?;
    step_mutation_response(&state, &headers, &id).await
}

pub(crate) async fn move_step_up(
    state: State<AppState>,
    path: Path<(String, String)>,
    headers: HeaderMap,
    form: Form<CsrfForm>,
) -> Result<Response, AppError> {
    move_step_by(state, path, headers, form, -1).await
}

pub(crate) async fn move_step_down(
    state: State<AppState>,
    path: Path<(String, String)>,
    headers: HeaderMap,
    form: Form<CsrfForm>,
) -> Result<Response, AppError> {
    move_step_by(state, path, headers, form, 1).await
}

async fn move_step_by(
    state: State<AppState>,
    Path((id, step_id)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
    direction: i64,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::combos::move_step(&state.db, &id, &step_id, direction).await?;
    step_mutation_response(&state, &headers, &id).await
}

/// HTMX always sends `hx-request: true` on boosted/element-triggered requests.
fn is_htmx_request(headers: &HeaderMap) -> bool {
    headers
        .get("hx-request")
        .is_some_and(|value| value == "true")
}

/// Fragment swap for HTMX callers, editor redirect for everyone else.
async fn step_mutation_response(
    state: &AppState,
    headers: &HeaderMap,
    combo_id: &str,
) -> Result<Response, AppError> {
    if is_htmx_request(headers) {
        Ok(steps_fragment(state, combo_id).await?.into_response())
    } else {
        Ok(see_other(&format!("/combos/{combo_id}")))
    }
}

async fn steps_fragment(state: &AppState, combo_id: &str) -> Result<Html<String>, AppError> {
    let steps = step_rows(state, combo_id).await?;
    render(ComboStepsFragment {
        csrf_token: state.csrf_token.as_str().to_owned(),
        combo_id: combo_id.to_string(),
        steps,
    })
}

/// Resolves each step's provider id to a display name (flagged when the
/// provider is disabled and therefore skipped by routing), falling back to the
/// id if the referenced provider no longer exists.
async fn step_rows(state: &AppState, combo_id: &str) -> Result<Vec<StepRow>, AppError> {
    let steps = crate::db::combos::steps(&state.db, combo_id).await?;
    let providers = crate::db::providers::list(&state.db).await?;
    let group_names = crate::db::provider_groups::connection_group_names(&state.db).await?;
    let names: HashMap<&str, (String, bool)> = providers
        .iter()
        .map(|p| {
            (
                p.id.as_str(),
                (connection_label(p, &group_names), p.enabled),
            )
        })
        .collect();
    Ok(steps
        .into_iter()
        .map(|step| {
            let (provider_name, provider_disabled) = match names.get(step.provider_id.as_str()) {
                Some((name, enabled)) => (name.clone(), !*enabled),
                None => (step.provider_id.clone(), false),
            };
            StepRow {
                id: step.id,
                provider_name,
                provider_disabled,
                model: step.model,
            }
        })
        .collect())
}

async fn provider_options(state: &AppState) -> Result<Vec<ProviderOption>, AppError> {
    let group_names = crate::db::provider_groups::connection_group_names(&state.db).await?;
    let mut options: Vec<ProviderOption> = crate::db::providers::list(&state.db)
        .await?
        .into_iter()
        .map(|p| {
            let name = connection_label(&p, &group_names);
            ProviderOption {
                id: p.id,
                name,
                disabled: !p.enabled,
            }
        })
        .collect();
    options.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(options)
}

fn connection_label(
    provider: &crate::db::providers::Provider,
    group_names: &HashMap<String, String>,
) -> String {
    let group_name = group_names.get(&provider.id).cloned().unwrap_or_else(|| {
        super::provider_catalog::display_name(&provider.base_url, &provider.protocol)
    });
    format!("{} / {}", group_name, provider.name)
}

/// Mirrors the DB constraint so obvious mistakes fail fast with a clear message.
fn validate_combo_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    let valid = (1..=NAME_MAX_CHARS).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if valid {
        Ok(name.to_string())
    } else {
        Err(AppError::InvalidRequest(format!(
            "combo name must be 1-{NAME_MAX_CHARS} characters of letters, digits, '_', '.', '-'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;
    use axum::http;

    const CSRF: &str = "csrf-test-token";

    #[test]
    fn name_validation_mirrors_db_constraints() {
        assert_eq!(
            validate_combo_name("  default.v2_1  ").unwrap(),
            "default.v2_1"
        );
        assert!(validate_combo_name("").is_err());
        assert!(validate_combo_name("   ").is_err());
        assert!(validate_combo_name("has space").is_err());
        assert!(validate_combo_name(&"x".repeat(65)).is_err());
        assert!(validate_combo_name(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn hx_request_header_detection() {
        let mut headers = http::HeaderMap::new();
        assert!(!is_htmx_request(&headers));
        headers.insert("hx-request", http::HeaderValue::from_static("false"));
        assert!(!is_htmx_request(&headers));
        headers.insert("hx-request", http::HeaderValue::from_static("true"));
        assert!(is_htmx_request(&headers));
        // Header name matching is case-insensitive per HTTP semantics.
        let mut odd_case = http::HeaderMap::new();
        odd_case.insert("HX-Request", http::HeaderValue::from_static("true"));
        assert!(is_htmx_request(&odd_case));
    }

    #[test]
    fn pages_render_steps_fragment_and_csrf() {
        let page = CombosPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            combos: vec![ComboRow {
                id: "default".to_string(),
                name: "default".to_string(),
                step_summary: "2 steps".to_string(),
            }],
        }
        .render()
        .unwrap();
        assert!(page.contains("<title>SparkRouter :: Combos</title>"));
        assert!(page.contains("action=\"/combos/default/delete\""));
        assert!(page.contains("name=\"_csrf\""));

        let new = ComboNewPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
        }
        .render()
        .unwrap();
        assert!(new.contains("action=\"/combos\""));
        assert!(new.contains("name=\"_csrf\""));

        let edit = ComboEditPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            combo_id: "default".to_string(),
            combo_name: "default".to_string(),
            steps: vec![StepRow {
                id: "s1".to_string(),
                provider_name: "Local".to_string(),
                provider_disabled: false,
                model: "qwen3 <chat>".to_string(),
            }],
            providers: vec![ProviderOption {
                id: "p1".to_string(),
                name: "Local".to_string(),
                disabled: true,
            }],
        }
        .render()
        .unwrap();
        assert!(edit.contains("action=\"/combos/default\""));
        assert!(edit.contains("hx-post=\"/combos/default/steps\""));
        assert!(edit.contains("qwen3 &#60;chat&#62;"));
        // Disabled providers are labeled as skipped in the add-step picker.
        assert!(edit.contains("<option value=\"p1\">Local (disabled)</option>"));
        // One step carries three mutation forms (up/down/remove), each with CSRF,
        // plus the rename form and the add-step form.
        assert_eq!(edit.matches("name=\"_csrf\"").count(), 5);

        let fragment = ComboStepsFragment {
            csrf_token: CSRF.to_string(),
            combo_id: "default".to_string(),
            steps: vec![
                StepRow {
                    id: "s1".to_string(),
                    provider_name: "A".to_string(),
                    provider_disabled: false,
                    model: "m1".to_string(),
                },
                StepRow {
                    id: "s2".to_string(),
                    provider_name: "B".to_string(),
                    provider_disabled: true,
                    model: "m2".to_string(),
                },
            ],
        }
        .render()
        .unwrap();
        // Boundary steps disable the corresponding move button; interior stays enabled.
        assert!(fragment.contains("disabled aria-label=\"Move step up\""));
        assert!(fragment.contains("disabled aria-label=\"Move step down\""));
        assert!(fragment.contains("aria-label=\"Move step up\">"));
        // Step provider labels flag routing-skipped providers; enabled ones stay bare.
        assert!(fragment.contains("<td>B (disabled)</td>"));
        assert!(fragment.contains("<td>A</td>"));
        // Two steps -> six forms, all carrying CSRF.
        assert_eq!(fragment.matches("name=\"_csrf\"").count(), 6);
        // Fragment is HTML-only, never a full document.
        assert!(!fragment.contains("<html"));
    }
}
