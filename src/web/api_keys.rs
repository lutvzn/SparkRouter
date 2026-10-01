//! Dashboard pages for scoped, optionally expiring gateway API keys.

use askama::Template;
use axum::{
    extract::{Form, Path, State},
    response::{Html, Response},
};
use secrecy::ExposeSecret;
use serde::Deserialize;

use super::{CsrfForm, check_csrf, render, see_other};
use crate::app::AppState;
use crate::db::api_keys::{Expiry, KeyAccess};
use crate::error::AppError;

#[derive(Template)]
#[template(path = "api_keys/list.html")]
struct ApiKeysPage {
    app_name: &'static str,
    csrf_token: String,
    keys: Vec<KeyRow>,
    available_models: Vec<String>,
}

struct KeyRow {
    id: String,
    name: String,
    prefix: String,
    created_at: String,
    restricted: bool,
    models: String,
    expiry: String,
    expired: bool,
}

#[derive(Template)]
#[template(path = "api_keys/created.html")]
struct CreatedPage {
    app_name: &'static str,
    name: String,
    secret: String,
    revealed: bool,
}

#[derive(Deserialize)]
pub(crate) struct CreateForm {
    _csrf: String,
    name: String,
    #[serde(flatten)]
    policy: PolicyForm,
}

#[derive(Deserialize)]
pub(crate) struct PolicyForm {
    #[serde(default)]
    access: String,
    #[serde(default)]
    models: String,
    #[serde(default)]
    expiry: String,
    #[serde(default)]
    expires_days: String,
}

#[derive(Deserialize)]
pub(crate) struct UpdateForm {
    _csrf: String,
    #[serde(flatten)]
    policy: PolicyForm,
}

fn policy(form: PolicyForm, editing: bool) -> Result<(KeyAccess, Expiry), AppError> {
    // Never silently discard submitted scopes on an unrestricted key.
    let access = match form.access.as_str() {
        "" if !editing && form.models.trim().is_empty() => KeyAccess::All,
        "all" if form.models.trim().is_empty() => KeyAccess::All,
        "selected" => {
            if form.models.len() > 210_000 {
                return Err(AppError::InvalidRequest(
                    "Allowed model list is too large".into(),
                ));
            }
            KeyAccess::Selected(
                form.models
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        }
        _ => {
            return Err(AppError::InvalidRequest(
                "Choose selected access when providing allowed model IDs".into(),
            ));
        }
    };
    let expiry =
        match form.expiry.as_str() {
            "keep" if editing && form.expires_days.trim().is_empty() => Expiry::Keep,
            "" if editing && form.expires_days.trim().is_empty() => Expiry::Keep,
            "" | "never" if form.expires_days.trim().is_empty() => Expiry::Never,
            "days" => Expiry::AfterDays(form.expires_days.trim().parse().map_err(|_| {
                AppError::InvalidRequest("Expiry must be 1–3650 days from now".into())
            })?),
            _ => {
                return Err(AppError::InvalidRequest(
                    "Choose a valid expiry option".into(),
                ));
            }
        };
    Ok((access, expiry))
}

pub(crate) async fn list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let keys = crate::db::api_keys::list(&state.db)
        .await?
        .into_iter()
        .map(|key| {
            let (restricted, models) = match key.access {
                KeyAccess::All => (false, String::new()),
                KeyAccess::Selected(models) => (true, models.join("\n")),
            };
            KeyRow {
                id: key.id,
                name: key.name,
                prefix: key.prefix,
                created_at: key.created_at,
                restricted,
                models,
                expired: key.expired,
                expiry: key.expires_at.unwrap_or_else(|| "Never".into()),
            }
        })
        .collect();
    let mut available_models: Vec<String> = crate::db::combos::list(&state.db)
        .await?
        .into_iter()
        .map(|combo| format!("combo/{}", combo.name))
        .collect();
    available_models.extend(crate::db::connection_models::routable_model_ids(&state.db).await?);
    render(ApiKeysPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        keys,
        available_models,
    })
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Form(form): Form<CreateForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let (access, expiry) = policy(form.policy, false)?;
    let issued = crate::db::api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        &form.name,
        access,
        expiry,
    )
    .await?;
    Ok(see_other(&format!("/api-keys/{}", issued.id)))
}

pub(crate) async fn update_policy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<UpdateForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let (access, expiry) = policy(form.policy, true)?;
    crate::db::api_keys::update_policy(&state.db, &id, access, expiry).await?;
    Ok(see_other("/api-keys"))
}

pub(crate) async fn created(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let secret = crate::db::api_keys::take_reveal(&state.db, &state.crypto, &id).await?;
    let key = crate::db::api_keys::list(&state.db)
        .await?
        .into_iter()
        .find(|key| key.id == id)
        .ok_or_else(|| AppError::NotFound(format!("API key '{id}' was not found")))?;
    render(CreatedPage {
        app_name: crate::APP_NAME,
        name: key.name,
        secret: secret
            .as_ref()
            .map(|value| value.expose_secret().to_owned())
            .unwrap_or_default(),
        revealed: secret.is_some(),
    })
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::api_keys::delete(&state.db, &id).await?;
    Ok(see_other("/api-keys"))
}
