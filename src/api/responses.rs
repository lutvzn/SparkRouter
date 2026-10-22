//! Calls on a stored Responses API response — retrieve, delete, cancel, and
//! list input items — forwarded to the connection that created it through
//! this gateway. Only the caller that created a response can reach it, and
//! only while its key still allows the model it was created with. These
//! calls generate nothing, so like `/v1/models` they skip traffic limits,
//! budgets, and request history.

use axum::{
    Extension,
    extract::{Path, RawQuery, State},
    response::Response,
};

use crate::{app::AppState, db::stored_responses, error::AppError};

pub(crate) async fn retrieve(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    forward(&state, &auth, id, reqwest::Method::GET, "", query).await
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    forward(&state, &auth, id, reqwest::Method::DELETE, "", None).await
}

pub(crate) async fn cancel(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    forward(&state, &auth, id, reqwest::Method::POST, "/cancel", None).await
}

pub(crate) async fn input_items(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    forward(
        &state,
        &auth,
        id,
        reqwest::Method::GET,
        "/input_items",
        query,
    )
    .await
}

/// Query strings are passed on as sent, limited to URL-safe characters.
fn query_suffix(query: Option<String>) -> Result<String, AppError> {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return Ok(String::new());
    };
    let safe = query.len() <= 2048
        && query.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'~' | b'%' | b'=' | b'&' | b'+' | b',' | b'[' | b']'
                )
        });
    if !safe {
        return Err(AppError::InvalidRequest(
            "Query parameters must be URL-encoded and at most 2048 bytes".into(),
        ));
    }
    Ok(format!("?{query}"))
}

async fn forward(
    state: &AppState,
    auth: &crate::app::Authorization,
    id: String,
    method: reqwest::Method,
    suffix: &str,
    query: Option<String>,
) -> Result<Response, AppError> {
    let missing = || {
        AppError::NotFound(
            "No stored response with this ID was created through this gateway for this key".into(),
        )
    };
    if !stored_responses::valid_id(&id) {
        return Err(missing());
    }
    // Retrieval can stream a background response (`stream=true`).
    let streaming = query.as_deref().is_some_and(|query| {
        query
            .split('&')
            .any(|pair| pair.eq_ignore_ascii_case("stream=true"))
    });
    let query = query_suffix(query)?;
    let stored = stored_responses::get(&state.db, &id, &auth.affinity_scope(state))
        .await?
        .ok_or_else(missing)?;
    if !auth.access.allows(&stored.model) {
        return Err(AppError::Forbidden);
    }
    let connection = state
        .config
        .connection(&state.db, &stored.provider_id)
        .await?
        .ok_or_else(missing)?;
    let provider = &connection.0;
    let responses = crate::routing::affinity::RequestContext::default()
        .for_endpoint(crate::routing::affinity::Endpoint::Responses);
    if !responses.accepts(provider) {
        return Err(missing());
    }
    let deleting = method == reqwest::Method::DELETE;
    let response = crate::providers::openai::stored_response(
        &state.client,
        &state.crypto,
        provider,
        method,
        &format!("responses/{id}{suffix}{query}"),
        streaming,
    )
    .await?;
    if deleting {
        state.history.forget_response(id);
    }
    Ok(response)
}
