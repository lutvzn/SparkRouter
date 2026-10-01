use crate::{config::Config, crypto::Crypto, db, error::AppError};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, Method, header},
    middleware::{self, Next},
    response::Response,
};
use secrecy::{ExposeSecret, SecretString};
use sqlx::SqlitePool;
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub client: reqwest::Client,
    pub crypto: Arc<Crypto>,
    pub api_key: Option<Arc<SecretString>>,
    pub csrf_token: Arc<String>,
    pub(crate) round_robin: Arc<crate::routing::combo::RoundRobin>,
}

impl AppState {
    pub async fn from_config(config: Config) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            // Never forward provider credentials to a redirect destination.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|_| AppError::HttpClient)?;
        Ok(Self {
            db: db::connect(&config.database_url).await?,
            client,
            crypto: Arc::new(config.crypto),
            api_key: config.api_key.map(Arc::new),
            csrf_token: Arc::new(uuid::Uuid::new_v4().to_string()),
            round_robin: Arc::default(),
        })
    }
}

pub fn router(state: AppState) -> Router {
    let api =
        crate::api::router().layer(middleware::from_fn_with_state(state.clone(), authenticate));
    // Dashboard is a distinct router so an authentication layer can be added here.
    let dashboard = crate::web::router().layer(middleware::from_fn(dashboard_origin));
    Router::new()
        .nest("/v1", api)
        .merge(dashboard)
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http().make_span_with(|request: &Request| {
            tracing::info_span!("http", method = %request.method(), request_id = ?request.headers().get("x-request-id"))
        }))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(state)
}

async fn authenticate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let env_match = state.api_key.as_ref().is_some_and(|expected| {
        bool::from(
            supplied
                .as_bytes()
                .ct_eq(expected.expose_secret().as_bytes()),
        )
    });
    if env_match || crate::db::api_keys::matches(&state.db, &state.crypto, supplied).await? {
        return Ok(next.run(request).await);
    }
    // Open only when neither the environment key nor a generated key exists.
    // A generated key is unrestricted and can call every combo.
    if state.api_key.is_none() && !crate::db::api_keys::any(&state.db).await? {
        return Ok(next.run(request).await);
    }
    Err(AppError::Unauthorized)
}

async fn dashboard_origin(request: Request, next: Next) -> Result<Response, AppError> {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        let cross_site = request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|h| h == "cross-site");
        let invalid_origin = request.headers().get(header::ORIGIN).is_some_and(|origin| {
            let origin = origin
                .to_str()
                .ok()
                .and_then(|s| reqwest::Url::parse(s).ok());
            let host = request
                .headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok());
            match (origin, host) {
                (Some(origin), Some(host)) => {
                    let expected = reqwest::Url::parse(&format!("{}://{host}", origin.scheme()));
                    !matches!(origin.scheme(), "http" | "https")
                        || expected.map_or(true, |expected| origin.origin() != expected.origin())
                }
                _ => true,
            }
        });
        if cross_site || invalid_origin {
            return Err(AppError::InvalidRequest(
                "Cross-origin dashboard submission rejected".into(),
            ));
        }
    }
    Ok(next.run(request).await)
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // `no-referrer` makes native form POSTs send Origin: null, which our
    // dashboard origin check correctly rejects. Keep same-origin information
    // for forms while still withholding referrers from external destinations.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'"));
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}
