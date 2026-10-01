//! Dashboard pages for credential-free configuration transfer.
//!
//! `GET /configuration` shows the export download and the import textarea.
//! `POST /configuration/preview` validates a pasted document and re-renders
//! the page with counts, warnings, conflicts, and a confirmation form that
//! carries the exact original text. `POST /configuration/import` applies it;
//! every check runs again inside the write transaction, so the preview can
//! never bypass validation or conflicts.

use askama::Template;
use axum::{
    extract::{Form, State},
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use super::{CsrfForm, check_csrf, provider_catalog, render, see_other};
use crate::app::AppState;
use crate::error::AppError;
use crate::providers::Protocol;

/// Fixed download name. It is a compile-time constant, never built from
/// stored or submitted values.
const EXPORT_FILENAME: &str = "attachment; filename=\"sparkrouter-configuration.json\"";

#[derive(Template)]
#[template(path = "config_transfer.html")]
pub(crate) struct ConfigTransferPage {
    app_name: &'static str,
    csrf_token: String,
    preview: Option<ImportPreview>,
}

/// One validated document rendered back to the operator. `configuration`
/// echoes the exact submitted text so the confirmation form cannot drift
/// from what was previewed.
pub(crate) struct ImportPreview {
    contents: crate::db::config_transfer::ConfigTransfer,
    groups: usize,
    connections: usize,
    combos: usize,
    steps: usize,
    enabled_connections: usize,
    warnings: Vec<String>,
    conflicts: Vec<String>,
    configuration: String,
}

/// Shared body of the preview and import actions: the CSRF token plus the
/// pasted configuration document.
#[derive(Deserialize)]
pub(crate) struct ConfigurationForm {
    _csrf: String,
    configuration: String,
}

/// Preset endpoints come from the dashboard catalog; the storage layer stays
/// free of catalog knowledge and receives this predicate instead, so
/// ungrouped non-preset connections can be given their own visible group.
fn is_preset_endpoint(base_url: &str, protocol: &str) -> bool {
    protocol
        .parse::<Protocol>()
        .map(|protocol| provider_catalog::matching(base_url, &protocol).is_some())
        .unwrap_or(false)
}

pub(crate) async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    render(ConfigTransferPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        preview: None,
    })
}

/// Exports the configuration as a JSON attachment. The document never
/// contains credentials, so no secret can leak through it.
pub(crate) async fn export(
    State(state): State<AppState>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let transfer = crate::db::config_transfer::export(&state.db).await?;
    let body = serde_json::to_string_pretty(&transfer)
        .map_err(|_| AppError::InvalidRequest("configuration could not be serialized".into()))?;
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static(EXPORT_FILENAME),
    );
    Ok(response)
}

pub(crate) async fn preview(
    State(state): State<AppState>,
    Form(form): Form<ConfigurationForm>,
) -> Result<Html<String>, AppError> {
    check_csrf(&state, &form._csrf)?;
    let check = crate::db::config_transfer::check_import(
        &state.db,
        &form.configuration,
        &is_preset_endpoint,
    )
    .await?;
    render(ConfigTransferPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        preview: Some(ImportPreview {
            contents: check.configuration,
            groups: check.stats.groups,
            connections: check.stats.connections,
            combos: check.stats.combos,
            steps: check.stats.steps,
            enabled_connections: check.stats.enabled_connections,
            warnings: check.warnings,
            conflicts: check.conflicts,
            configuration: form.configuration,
        }),
    })
}

/// Applies a previously previewed document. The document is revalidated and
/// rechecked for combo-name conflicts inside the write transaction; success
/// redirects to the providers page where each disabled connection must be
/// reviewed, keyed, and enabled.
pub(crate) async fn import(
    State(state): State<AppState>,
    Form(form): Form<ConfigurationForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::config_transfer::apply(&state.db, &form.configuration, &is_preset_endpoint).await?;
    Ok(see_other("/providers"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    const CSRF: &str = "csrf-test-token";

    fn preview_view(configuration: &str) -> ImportPreview {
        ImportPreview {
            contents: crate::db::config_transfer::ConfigTransfer {
                version: 1,
                groups: vec![],
                connections: vec![],
                combos: vec![],
            },
            groups: 2,
            connections: 3,
            combos: 1,
            steps: 4,
            enabled_connections: 2,
            warnings: vec!["Every imported connection starts disabled with no API key.".to_owned()],
            conflicts: vec![],
            configuration: configuration.to_owned(),
        }
    }

    #[test]
    fn page_renders_export_and_import_forms_with_csrf() {
        let page = ConfigTransferPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            preview: None,
        }
        .render()
        .unwrap();
        assert!(page.contains("<title>SparkRouter :: Configuration</title>"));
        assert!(page.contains("action=\"/configuration/export\""));
        assert!(page.contains("action=\"/configuration/preview\""));
        assert!(page.contains("name=\"configuration\""));
        assert!(!page.contains("action=\"/configuration/import\""));
        assert_eq!(page.matches("name=\"_csrf\"").count(), 2);
    }

    #[test]
    fn preview_renders_counts_warnings_and_a_confirmation_form() {
        let json = r#"{"version":1,"groups":[{"id":"g","name":"Work <LLM>"}]}"#;
        let page = ConfigTransferPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            preview: Some(preview_view(json)),
        }
        .render()
        .unwrap();
        assert!(page.contains("Import preview"));
        assert!(page.contains("2 groups"));
        assert!(page.contains("3 connections"));
        assert!(page.contains("1 combos with 4 steps"));
        assert!(page.contains("starts disabled"));
        assert!(page.contains("action=\"/configuration/import\""));
        // The confirmation form carries the original document and the CSRF
        // token, with user-controlled names and JSON quotes escaped.
        assert!(page.contains("name=\"configuration\""));
        assert!(page.contains("Work &#60;LLM&#62;"));
        assert!(page.contains("{&#34;version&#34;:1"));
        assert_eq!(page.matches("name=\"_csrf\"").count(), 3);
    }

    #[test]
    fn conflicting_names_replace_the_confirmation_form() {
        let page = ConfigTransferPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            preview: Some(ImportPreview {
                conflicts: vec!["chain".to_owned()],
                ..preview_view("{}")
            }),
        }
        .render()
        .unwrap();
        assert!(!page.contains("action=\"/configuration/import\""));
        assert!(page.contains("already exists"));
        assert!(page.contains("<code>chain</code>"));
    }
}
