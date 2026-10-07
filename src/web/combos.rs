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
use crate::db::{
    combos::{self, Strategy},
    provider_groups::ConnectionGroup,
    providers::Provider,
};
use crate::error::AppError;

const NAME_MAX_CHARS: usize = combos::MAX_NAME_LENGTH;

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
    strategy_label: String,
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
    /// Stored strategy as its form value (`as_str`) so the select can mark
    /// the matching option selected.
    strategy: String,
    steps: Vec<StepRow>,
    provider_groups: Vec<ProviderGroup>,
    providers: Vec<ProviderOption>,
}

/// HTMX swap target for step mutations; also included by the full edit page.
#[derive(Template)]
#[template(path = "combos/_steps.html")]
pub(crate) struct ComboStepsFragment {
    csrf_token: String,
    combo_id: String,
    steps: Vec<StepRow>,
    provider_groups: Vec<ProviderGroup>,
    providers: Vec<ProviderOption>,
}

pub(crate) struct StepRow {
    id: String,
    provider_id: String,
    group_id: String,
    provider_name: String,
    /// True when the referenced provider is disabled and skipped by routing.
    provider_disabled: bool,
    model: String,
}

pub(crate) struct ProviderGroup {
    id: String,
    name: String,
}

pub(crate) struct ProviderOption {
    id: String,
    /// Stable provider key shared by every connection in that dropdown group.
    group_id: String,
    /// Connection name only. The provider is chosen in the other dropdown.
    connection_name: String,
    /// Appended as "(disabled)" in the picker so skipped upstreams are visible.
    disabled: bool,
    /// Last refreshed model IDs for this connection. Empty when never refreshed.
    models: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ComboForm {
    _csrf: String,
    name: String,
    /// Absent for legacy name-only posts: creation then defaults to
    /// `priority`, while edits preserve the stored strategy.
    #[serde(default)]
    strategy: Option<String>,
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
            strategy_label: combo.strategy.label().to_owned(),
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
    // Parse before the insert so an unknown value is a 400, never a row.
    let strategy = parse_strategy(form.strategy.as_deref())?.unwrap_or(Strategy::Priority);
    let combo = crate::db::combos::create_with_strategy(&state.db, &name, strategy).await?;
    Ok(see_other(&format!("/combos/{}", combo.id)))
}

pub(crate) async fn edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let combo = crate::db::combos::get(&state.db, &id).await?;
    let (steps, provider_groups, providers) = editor_data(&state, &combo.id).await?;
    render(ComboEditPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        combo_id: combo.id,
        combo_name: combo.name,
        strategy: combo.strategy.as_str().to_owned(),
        steps,
        provider_groups,
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
    // Parse before the write so an unknown value is a 400 without applying
    // a partial rename; `None` preserves the stored strategy.
    let strategy = parse_strategy(form.strategy.as_deref())?;
    crate::db::combos::update(&state.db, &id, &name, strategy).await?;
    Ok(see_other(&format!("/combos/{id}")))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::combos::delete(&state.db, &id).await?;
    // The combo's rotation cursor can no longer be reused; drop it so a
    // future combo with the same id starts fresh.
    state.round_robin.forget(&id);
    Ok(see_other("/combos"))
}

/// Duplicates a combo (unique `source-copy` name, cloned steps) and redirects
/// to the new combo's editor so the copy can be adjusted right away.
pub(crate) async fn duplicate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let combo = crate::db::combos::duplicate(&state.db, &id).await?;
    Ok(see_other(&format!("/combos/{}", combo.id)))
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
    let model = combos::validate_model(&form.model).map_err(|_| {
        AppError::InvalidRequest(if form.model.trim().is_empty() {
            "model is required".into()
        } else {
            format!(
                "model must be at most {} characters",
                combos::MAX_MODEL_LENGTH
            )
        })
    })?;
    combos::add_step(&state.db, &id, provider_id, &model).await?;
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

pub(crate) async fn edit_step(
    State(state): State<AppState>,
    Path((id, step_id)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<StepForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::combos::update_step(
        &state.db,
        &id,
        &step_id,
        form.provider_id.trim(),
        &form.model,
    )
    .await?;
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
    let (steps, provider_groups, providers) = editor_data(state, combo_id).await?;
    render(ComboStepsFragment {
        csrf_token: state.csrf_token.as_str().to_owned(),
        combo_id: combo_id.to_string(),
        steps,
        provider_groups,
        providers,
    })
}

async fn editor_data(
    state: &AppState,
    combo_id: &str,
) -> Result<(Vec<StepRow>, Vec<ProviderGroup>, Vec<ProviderOption>), AppError> {
    let providers = crate::db::providers::list(&state.db).await?;
    let memberships = crate::db::provider_groups::connection_groups(&state.db).await?;
    let steps = step_rows(state, combo_id, &providers, &memberships).await?;
    let (groups, options) = provider_options(state, providers, &memberships).await?;
    Ok((steps, groups, options))
}

/// Resolves each step's provider id to a display name (flagged when the
/// provider is disabled and therefore skipped by routing), falling back to the
/// id if the referenced provider no longer exists.
async fn step_rows(
    state: &AppState,
    combo_id: &str,
    providers: &[Provider],
    memberships: &HashMap<String, ConnectionGroup>,
) -> Result<Vec<StepRow>, AppError> {
    let steps = crate::db::combos::steps(&state.db, combo_id).await?;
    let groups: HashMap<&str, String> = providers
        .iter()
        .map(|p| (p.id.as_str(), provider_group(p, memberships).0))
        .collect();
    let names: HashMap<&str, (String, bool)> = providers
        .iter()
        .map(|p| (p.id.as_str(), (connection_label(p, memberships), p.enabled)))
        .collect();
    Ok(steps
        .into_iter()
        .map(|step| {
            let (provider_name, provider_disabled) = match names.get(step.provider_id.as_str()) {
                Some((name, enabled)) => (name.clone(), !*enabled),
                None => (step.provider_id.clone(), false),
            };
            StepRow {
                group_id: groups
                    .get(step.provider_id.as_str())
                    .cloned()
                    .unwrap_or_default(),
                provider_id: step.provider_id,
                id: step.id,
                provider_name,
                provider_disabled,
                model: step.model,
            }
        })
        .collect())
}

async fn provider_options(
    state: &AppState,
    stored: Vec<Provider>,
    memberships: &HashMap<String, ConnectionGroup>,
) -> Result<(Vec<ProviderGroup>, Vec<ProviderOption>), AppError> {
    let ids: Vec<String> = stored.iter().map(|provider| provider.id.clone()).collect();
    let lists = crate::db::connection_models::for_connections(&state.db, &ids).await?;
    let mut groups = HashMap::new();
    let mut options: Vec<ProviderOption> = stored
        .into_iter()
        .map(|p| {
            let models = lists
                .get(&p.id)
                .map(|list| list.ids.clone())
                .unwrap_or_default();
            let (group_id, group_name) = provider_group(&p, memberships);
            groups.entry(group_id.clone()).or_insert(group_name);
            ProviderOption {
                id: p.id,
                group_id,
                connection_name: p.name,
                disabled: !p.enabled,
                models,
            }
        })
        .collect();
    options.sort_by(|a, b| {
        groups
            .get(&a.group_id)
            .cmp(&groups.get(&b.group_id))
            .then_with(|| a.connection_name.cmp(&b.connection_name))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut provider_groups: Vec<ProviderGroup> = groups
        .into_iter()
        .map(|(id, name)| ProviderGroup { id, name })
        .collect();
    provider_groups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    disambiguate_group_names(&mut provider_groups);
    Ok((provider_groups, options))
}

/// Custom providers stay separate even when their names match. Preset
/// connections group by catalog slug. Anything else groups by endpoint.
fn provider_group(
    provider: &crate::db::providers::Provider,
    memberships: &HashMap<String, crate::db::provider_groups::ConnectionGroup>,
) -> (String, String) {
    if let Some(group) = memberships.get(&provider.id) {
        return (format!("group:{}", group.id), group.name.clone());
    }
    if let Some(preset) = super::provider_catalog::matching(&provider.base_url, &provider.protocol)
    {
        return (format!("preset:{}", preset.slug), preset.name.to_string());
    }
    (
        format!(
            "endpoint:{}|{}",
            provider.base_url,
            provider.protocol.as_str()
        ),
        super::provider_catalog::display_name(&provider.base_url, &provider.protocol),
    )
}

fn disambiguate_group_names(groups: &mut [ProviderGroup]) {
    let mut seen = HashMap::<String, usize>::new();
    for group in groups.iter_mut() {
        let count = seen.entry(group.name.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            group.name = format!("{} ({count})", group.name);
        }
    }
}

fn connection_label(
    provider: &crate::db::providers::Provider,
    memberships: &HashMap<String, ConnectionGroup>,
) -> String {
    let group_name = memberships
        .get(&provider.id)
        .map(|group| group.name.clone())
        .unwrap_or_else(|| {
            super::provider_catalog::display_name(&provider.base_url, &provider.protocol)
        });
    format!("{} / {}", group_name, provider.name)
}

/// Parses the optional strategy form field, rejecting unknown values before
/// any database write. A missing or blank value yields `None`: creation then
/// defaults to `priority`, while updates preserve the stored strategy.
fn parse_strategy(raw: Option<&str>) -> Result<Option<Strategy>, AppError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => Strategy::parse(value).map(Some),
    }
}

/// Mirrors the DB constraint so obvious mistakes fail fast with a clear message.
fn validate_combo_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    combos::validate_name(name).map_err(|_| {
        AppError::InvalidRequest(format!(
            "combo name must be 1-{NAME_MAX_CHARS} characters of letters, digits, '_', '.', '-'"
        ))
    })?;
    Ok(name.to_owned())
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
    fn strategy_parsing_defaults_only_when_blank() {
        // Absent and blank mean "not supplied": creation defaults, edits keep
        // the stored value. Anything else must parse exactly.
        assert!(parse_strategy(None).unwrap().is_none());
        assert!(parse_strategy(Some("")).unwrap().is_none());
        assert!(parse_strategy(Some("   ")).unwrap().is_none());
        assert_eq!(
            parse_strategy(Some("priority"))
                .unwrap()
                .map(|s| s.as_str()),
            Some("priority")
        );
        assert_eq!(
            parse_strategy(Some(" round_robin "))
                .unwrap()
                .map(|s| s.as_str()),
            Some("round_robin")
        );
        assert_eq!(
            parse_strategy(Some("random")).unwrap().map(|s| s.as_str()),
            Some("random")
        );
        for bad in ["bogus", "roundrobin", "round-robin", "random!"] {
            assert!(
                parse_strategy(Some(bad)).is_err(),
                "strategy {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn provider_groups_keep_custom_providers_separate_from_presets() {
        use crate::db::provider_groups::ConnectionGroup;
        use crate::db::providers::Provider;
        use crate::providers::Protocol;

        let custom = Provider {
            id: "conn".into(),
            name: "Personal".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: Vec::new(),
            enabled: true,
        };
        let memberships = HashMap::from([(
            "conn".into(),
            ConnectionGroup {
                id: "custom-1".into(),
                name: "OpenRouter".into(),
            },
        )]);
        assert_eq!(
            provider_group(&custom, &memberships),
            ("group:custom-1".into(), "OpenRouter".into())
        );
        assert_eq!(
            provider_group(&custom, &HashMap::new()),
            ("preset:openrouter".into(), "OpenRouter".into())
        );
        let mut groups = vec![
            ProviderGroup {
                id: "a".into(),
                name: "Work".into(),
            },
            ProviderGroup {
                id: "b".into(),
                name: "Work".into(),
            },
        ];
        disambiguate_group_names(&mut groups);
        assert_eq!(groups[0].name, "Work");
        assert_eq!(groups[1].name, "Work (2)");
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
                strategy_label: "Round robin".to_string(),
            }],
        }
        .render()
        .unwrap();
        assert!(page.contains("<title>SparkRouter :: Combos</title>"));
        assert!(page.contains("action=\"/combos/default/delete\""));
        assert!(page.contains("name=\"_csrf\""));
        // Rows carry the step summary plus a strategy badge.
        assert!(page.contains(">2 steps</span>"));
        assert!(page.contains("<span class=\"badge\">Round robin</span>"));
        // Duplicate posts separately; Copy exposes the routing alias to the
        // embedded script without submitting a mutation or inline JavaScript.
        assert!(page.contains("action=\"/combos/default/duplicate\""));
        assert!(page.contains("data-copy-model=\"combo/default\""));
        let copy_label = page
            .find("<span class=\"btn-label\">Copy</span>")
            .expect("Copy label renders");
        let copy_start = page[..copy_label]
            .rfind("<button")
            .expect("Copy button element start");
        let copy_end = copy_label
            + page[copy_label..]
                .find("</button>")
                .expect("Copy button closes")
            + "</button>".len();
        let copy_button = &page[copy_start..copy_end];
        assert!(copy_button.contains("href=\"/static/icons.svg#copy\""));
        assert!(
            copy_button.contains("type=\"button\""),
            "Copy must not submit a mutation form"
        );
        assert!(
            !copy_button.contains("onclick") && !page.contains("onclick"),
            "the Copy button must not use inline JavaScript"
        );
        // One row carries two mutation forms (duplicate/delete), each with CSRF.
        assert_eq!(page.matches("name=\"_csrf\"").count(), 2);

        let new = ComboNewPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
        }
        .render()
        .unwrap();
        assert!(new.contains("action=\"/combos\""));
        assert!(new.contains("name=\"_csrf\""));
        // The strategy select defaults to priority and offers all three.
        assert!(new.contains("<select id=\"strategy\" name=\"strategy\""));
        assert!(new.contains("<option value=\"priority\" selected>Priority</option>"));
        assert!(new.contains("<option value=\"round_robin\">Round robin</option>"));
        assert!(new.contains("<option value=\"random\">Random</option>"));

        let edit = ComboEditPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            combo_id: "default".to_string(),
            combo_name: "default".to_string(),
            strategy: "round_robin".to_string(),
            steps: vec![StepRow {
                id: "s1".to_string(),
                provider_id: "p1".to_string(),
                group_id: "preset:local".to_string(),
                provider_name: "Local".to_string(),
                provider_disabled: false,
                model: "qwen3 <chat>".to_string(),
            }],
            provider_groups: vec![ProviderGroup {
                id: "preset:local".to_string(),
                name: "Local".to_string(),
            }],
            providers: vec![ProviderOption {
                id: "p1".to_string(),
                group_id: "preset:local".to_string(),
                connection_name: "Personal".to_string(),
                disabled: true,
                models: vec!["local-model".to_string()],
            }],
        }
        .render()
        .unwrap();
        assert!(edit.contains("action=\"/combos/default\""));
        assert!(edit.contains("hx-post=\"/combos/default/steps\""));
        assert!(edit.contains("qwen3 &#60;chat&#62;"));
        // The stored strategy marks exactly one option selected.
        assert!(edit.contains("<option value=\"round_robin\" selected>Round robin</option>"));
        assert!(
            !edit.contains("value=\"priority\" selected")
                && !edit.contains("value=\"random\" selected"),
            "only the stored strategy may be selected"
        );
        assert!(edit.contains("Save changes"));
        // Provider and connection are separate pickers. The connection still
        // posts its id, and a disabled connection stays labeled as skipped.
        assert!(edit.contains("<label for=\"provider_group\">Provider</label>"));
        assert!(edit.contains("<label for=\"provider_id\">Connection</label>"));
        assert!(edit.contains("<option value=\"preset:local\">Local</option>"));
        assert!(edit.contains(
            "<option value=\"p1\" data-provider=\"preset:local\">Personal (disabled)</option>"
        ));
        // One step carries four mutation forms (up/down/remove/edit), each with CSRF,
        // plus the rename form and the add-step form.
        assert_eq!(edit.matches("name=\"_csrf\"").count(), 6);

        let fragment = ComboStepsFragment {
            csrf_token: CSRF.to_string(),
            combo_id: "default".to_string(),
            provider_groups: vec![],
            providers: vec![],
            steps: vec![
                StepRow {
                    id: "s1".to_string(),
                    provider_id: "p1".to_string(),
                    group_id: "preset:local".to_string(),
                    provider_name: "A".to_string(),
                    provider_disabled: false,
                    model: "m1".to_string(),
                },
                StepRow {
                    id: "s2".to_string(),
                    provider_id: "p2".to_string(),
                    group_id: "preset:local".to_string(),
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
        // Two steps -> eight forms, all carrying CSRF.
        assert_eq!(fragment.matches("name=\"_csrf\"").count(), 8);
        // Fragment is HTML-only, never a full document.
        assert!(!fragment.contains("<html"));
    }
}
