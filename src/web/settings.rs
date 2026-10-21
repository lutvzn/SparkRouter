//! Small administration surface; deployment secrets are never rendered or edited.
use super::{check_csrf, render, see_other};
use crate::{app::AppState, db, error::AppError};
use askama::Template;
use axum::{
    extract::{Form, Query, State},
    response::{Html, Response},
};
use serde::Deserialize;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    app_name: &'static str,
    csrf_token: String,
    retention: String,
    logging_mode: String,
    history_buffer: String,
    notice: &'static str,
    dashboard_auth: &'static str,
    password_changeable: bool,
    password_error: &'static str,
    api_auth: &'static str,
    database_status: &'static str,
    version: &'static str,
    uptime: String,
    memory: String,
    preview: Option<super::config_transfer::ImportPreview>,
}

#[derive(Deserialize)]
pub(crate) struct SettingsQuery {
    result: Option<String>,
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Query(query): Query<SettingsQuery>,
) -> Result<Html<String>, AppError> {
    render_page(&state, query.result.as_deref(), None).await
}

pub(super) async fn render_page(
    state: &AppState,
    result: Option<&str>,
    preview: Option<super::config_transfer::ImportPreview>,
) -> Result<Html<String>, AppError> {
    let ready = crate::health::database_ready(&state.db).await;
    let (retention, api_auth) = if ready {
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let retention = db::settings::history_retention(&state.db).await?;
            let keys = db::api_keys::any(&state.db).await?;
            Ok::<_, AppError>((retention, keys))
        })
        .await;
        match result {
            Ok(Ok((retention, keys))) => (
                retention.to_string(),
                if state.api_key.is_some() || keys {
                    "Enabled"
                } else {
                    "Disabled — API is open"
                },
            ),
            _ => (
                String::new(),
                if state.api_key.is_some() {
                    "Enabled"
                } else {
                    "Unavailable"
                },
            ),
        }
    } else {
        (
            String::new(),
            if state.api_key.is_some() {
                "Enabled"
            } else {
                "Unavailable"
            },
        )
    };
    let logging_mode = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        db::settings::logging_mode(&state.db),
    )
    .await
    {
        Ok(Ok(mode)) => mode.as_str().to_owned(),
        _ => String::new(),
    };
    let history_buffer = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        db::settings::history_buffer(&state.db),
    )
    .await
    {
        Ok(Ok(scale)) => scale.to_string(),
        _ => String::new(),
    };
    let elapsed = state.started_at.elapsed().as_secs();
    render(SettingsPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        retention,
        logging_mode,
        history_buffer,
        preview,
        notice: match result {
            Some("saved") => "History retention saved.",
            Some("logging") => "Request logging mode saved. Existing history is unchanged.",
            Some("buffer") => "History write buffer saved. It applies immediately.",
            Some("cleared") => "History cleared. Dashboard usage totals have been reset.",
            Some("password") => "Dashboard password changed. Other sessions were signed out.",
            _ => "",
        },
        dashboard_auth: state.dashboard_auth.as_ref().map_or(
            "Disabled by ROUTER_DASHBOARD_AUTH — dashboard is open",
            |auth| auth.describe(),
        ),
        password_changeable: state
            .dashboard_auth
            .as_ref()
            .is_some_and(|auth| auth.changeable()),
        password_error: match result {
            Some("password-wrong") => "The current password is incorrect.",
            Some(code) => super::auth::PasswordProblem::from_code(code)
                .map_or("", super::auth::PasswordProblem::message),
            None => "",
        },
        api_auth,
        database_status: if ready { "Connected" } else { "Unavailable" },
        version: crate::APP_VERSION,
        uptime: format!(
            "{}d {}h {}m {}s",
            elapsed / 86400,
            elapsed / 3600 % 24,
            elapsed / 60 % 60,
            elapsed % 60
        ),
        memory: super::home::process_memory().await,
    })
}

#[derive(Deserialize)]
pub(crate) struct RetentionForm {
    _csrf: String,
    history_retention: String,
}

pub(crate) async fn save(
    State(state): State<AppState>,
    Form(form): Form<RetentionForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let retention = form.history_retention.parse().map_err(|_| {
        AppError::InvalidRequest(
            "history retention must be a whole number between 1 and 10,000".into(),
        )
    })?;
    db::settings::set_history_retention(&state.db, retention).await?;
    Ok(see_other("/settings?result=saved"))
}

#[derive(Deserialize)]
pub(crate) struct LoggingForm {
    _csrf: String,
    logging_mode: String,
    confirm_extended: Option<String>,
}

pub(crate) async fn save_logging(
    State(state): State<AppState>,
    Form(form): Form<LoggingForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let mode = db::settings::LoggingMode::parse(&form.logging_mode)?;
    if mode == db::settings::LoggingMode::Extended
        && form.confirm_extended.as_deref() != Some("yes")
    {
        return Err(AppError::InvalidRequest(
            "confirm sensitive body storage before enabling Extended logging".into(),
        ));
    }
    db::settings::set_logging_mode(&state.db, mode).await?;
    Ok(see_other("/settings?result=logging"))
}

#[derive(Deserialize)]
pub(crate) struct HistoryBufferForm {
    _csrf: String,
    history_buffer: String,
}

/// Saved and applied to the running writer at once; startup reapplies it.
pub(crate) async fn save_history_buffer(
    State(state): State<AppState>,
    Form(form): Form<HistoryBufferForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let scale = form.history_buffer.parse().map_err(|_| {
        AppError::InvalidRequest(
            "history write buffer must be 1, 2, 4, or 8 times the default".into(),
        )
    })?;
    db::settings::set_history_buffer(&state.db, scale).await?;
    state.history.set_buffer(scale);
    Ok(see_other("/settings?result=buffer"))
}

#[derive(Deserialize)]
pub(crate) struct ClearForm {
    _csrf: String,
    confirm: Option<String>,
}

pub(crate) async fn clear_history(
    State(state): State<AppState>,
    Form(form): Form<ClearForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    if form.confirm.as_deref() != Some("yes") {
        return Err(AppError::InvalidRequest(
            "confirm clearing history first".into(),
        ));
    }
    db::request_history::clear(&state.db).await?;
    Ok(see_other("/settings?result=cleared"))
}
