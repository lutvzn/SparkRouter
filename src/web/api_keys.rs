//! Dashboard pages for unrestricted gateway API keys.

use askama::Template;
use axum::{
    extract::{Form, Path, State},
    response::{Html, Response},
};
use secrecy::ExposeSecret;
use serde::Deserialize;

use super::{CsrfForm, check_csrf, render, see_other};
use crate::app::AppState;
use crate::error::AppError;

#[derive(Template)]
#[template(path = "api_keys/list.html")]
struct ApiKeysPage {
    app_name: &'static str,
    csrf_token: String,
    keys: Vec<KeyRow>,
}

struct KeyRow {
    id: String,
    name: String,
    prefix: String,
    created_at: String,
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
}

pub(crate) async fn list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let keys = crate::db::api_keys::list(&state.db)
        .await?
        .into_iter()
        .map(|key| KeyRow {
            id: key.id,
            name: key.name,
            prefix: key.prefix,
            created_at: key.created_at,
        })
        .collect();
    render(ApiKeysPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        keys,
    })
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Form(form): Form<CreateForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let issued = crate::db::api_keys::create(&state.db, &state.crypto, &form.name).await?;
    Ok(see_other(&format!("/api-keys/{}", issued.id)))
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
