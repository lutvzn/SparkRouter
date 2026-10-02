//! Server-rendered admin UI for managing providers and combos.
//!
//! Plain Askama templates rendered to strings and wrapped in `axum::response::Html`
//! (no `askama_axum` dependency). Interactivity that benefits from it (connection
//! tests, step reordering) uses a self-hosted, version-pinned HTMX asset served
//! from `/static/...`; management forms work without JavaScript too. The test
//! chat uses a small embedded script for page-local conversation history.

mod api_keys;
pub(crate) mod auth;
mod combos;
mod config_transfer;
mod history;
pub(crate) use crate::provider_catalog;
mod providers;
mod test;

use askama::Template;
use axum::{
    Router,
    extract::Path,
    http,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rust_embed::RustEmbed;
use serde::Deserialize;

use crate::app::AppState;
use crate::error::AppError;

/// Embedded admin assets: pinned HTMX, dashboard scripts, and stylesheet.
/// Compiled into the executable in debug and release builds.
#[derive(RustEmbed)]
#[folder = "static/"]
struct StaticAssets;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(auth::page).post(auth::login))
        .route("/logout", get(auth::logout_page).post(auth::logout))
        .route("/history", get(history::page))
        .route("/history/clear", post(history::clear))
        .route("/configuration", get(config_transfer::page))
        .route("/configuration/export", post(config_transfer::export))
        .route("/configuration/preview", post(config_transfer::preview))
        .route("/configuration/import", post(config_transfer::import))
        .route("/", get(|| async { Redirect::to("/providers") }))
        .route("/providers", get(providers::list).post(providers::create))
        .route("/providers/new", get(providers::new))
        .route("/providers/custom", post(providers::custom_create))
        .route(
            "/providers/catalog/{slug}",
            get(providers::catalog).post(providers::catalog_create),
        )
        .route(
            "/providers/groups/{id}",
            get(providers::group).post(providers::group_create),
        )
        .route(
            "/providers/groups/{id}/delete",
            post(providers::delete_group),
        )
        .route(
            "/providers/{id}",
            get(providers::edit).post(providers::update),
        )
        .route("/providers/{id}/delete", post(providers::delete))
        .route("/providers/{id}/test", post(providers::test))
        .route("/providers/{id}/models", post(providers::refresh_models))
        .route("/api-keys", get(api_keys::list).post(api_keys::create))
        .route("/api-keys/{id}", get(api_keys::created))
        .route("/api-keys/{id}/delete", post(api_keys::delete))
        .route("/api-keys/{id}/policy", post(api_keys::update_policy))
        .route("/combos", get(combos::list).post(combos::create))
        .route("/test", get(test::page))
        .route("/test/chat", post(test::chat))
        .route("/combos/new", get(combos::new))
        .route("/combos/{id}", get(combos::edit).post(combos::rename))
        .route("/combos/{id}/delete", post(combos::delete))
        .route("/combos/{id}/duplicate", post(combos::duplicate))
        .route("/combos/{id}/steps", post(combos::add_step))
        .route("/combos/{id}/steps/{step_id}/edit", post(combos::edit_step))
        .route(
            "/combos/{id}/steps/{step_id}/remove",
            post(combos::remove_step),
        )
        .route(
            "/combos/{id}/steps/{step_id}/up",
            post(combos::move_step_up),
        )
        .route(
            "/combos/{id}/steps/{step_id}/down",
            post(combos::move_step_down),
        )
        .route("/static/{*path}", get(static_asset))
}

/// Compares a submitted form token against the process-wide CSRF token in
/// constant time. The token is random per process, not secret-derived, but a
/// constant-time comparison avoids leaking its length-validated prefix anyway.
pub fn check_csrf(state: &AppState, token: &str) -> Result<(), AppError> {
    use subtle::ConstantTimeEq as _;
    let expected = state.csrf_token.as_bytes();
    let provided = token.as_bytes();
    let matches = expected.len() == provided.len() && bool::from(expected.ct_eq(provided));
    if matches {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(
            "missing or invalid CSRF token; reload the page and try again".into(),
        ))
    }
}

/// Minimal form body carrying only the CSRF token (delete/test/move buttons).
#[derive(Debug, Deserialize)]
pub(crate) struct CsrfForm {
    _csrf: String,
}

/// Renders an Askama template to an HTML response, mapping failures to `AppError`.
pub(crate) fn render<T: Template>(template: T) -> Result<Html<String>, AppError> {
    Ok(Html(template.render()?))
}

/// 303 redirect so browsers switch to GET after a form POST.
pub(crate) fn see_other(uri: &str) -> Response {
    (
        http::StatusCode::SEE_OTHER,
        [(http::header::LOCATION, uri.to_string())],
    )
        .into_response()
}

async fn static_asset(Path(path): Path<String>) -> Result<Response, AppError> {
    // Embedded keys always use forward-slash-relative names.
    if path.starts_with('/') || path.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(AppError::NotFound("static asset not found".into()));
    }
    let asset = StaticAssets::get(&path)
        .ok_or_else(|| AppError::NotFound("static asset not found".into()))?;
    Ok((
        [
            (http::header::CONTENT_TYPE, static_content_type(&path)),
            (
                http::header::CACHE_CONTROL,
                if path == "htmx-2.0.4.min.js" {
                    "public, max-age=604800"
                } else {
                    "public, max-age=0, must-revalidate"
                },
            ),
        ],
        asset.data.into_owned(),
    )
        .into_response())
}

fn static_content_type(path: &str) -> &'static str {
    let extension = path.rsplit('.').next().unwrap_or_default();
    match extension.to_ascii_lowercase().as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn csrf_comparison_is_exact() {
        use std::sync::{Arc, OnceLock};

        use base64::{Engine, engine::general_purpose::STANDARD};
        use secrecy::SecretString;

        use crate::app::AppState;
        use crate::crypto::Crypto;

        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let runtime = RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().unwrap());
        let db = runtime
            .block_on(sqlx::SqlitePool::connect("sqlite::memory:"))
            .unwrap();
        let state = AppState {
            db,
            client: reqwest::Client::new(),
            crypto: Arc::new(
                Crypto::from_base64(&SecretString::from(STANDARD.encode([3u8; 32]))).unwrap(),
            ),
            api_key: None,
            dashboard_auth: None,
            csrf_token: Arc::new("expected-token".to_string()),
            round_robin: Arc::default(),
        };
        assert!(super::check_csrf(&state, "expected-token").is_ok());
        assert!(super::check_csrf(&state, "expected-toke").is_err());
        assert!(super::check_csrf(&state, "").is_err());
        assert!(super::check_csrf(&state, "expected-token-extra").is_err());
    }
}
