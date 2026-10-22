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
    pub(crate) started_at: std::time::Instant,
    pub db: SqlitePool,
    /// Queue for the group-commit request-history writer, which owns the
    /// connection from [`db::connect_history_writer`]. Reads and all other
    /// writes use `db`.
    pub(crate) history: db::history_writer::HistoryWriter,
    /// Request-path configuration, invalidated by every commit on `db`.
    pub(crate) config: Arc<db::config_cache::ConfigCache>,
    pub client: reqwest::Client,
    pub crypto: Arc<Crypto>,
    pub api_key: Option<Arc<SecretString>>,
    pub csrf_token: Arc<String>,
    pub dashboard_auth: Option<Arc<crate::web::auth::DashboardAuth>>,
    pub(crate) key_limits: Arc<crate::key_limits::Limiter>,
    pub(crate) spending: Arc<crate::key_limits::Spending>,
    pub(crate) round_robin: Arc<crate::routing::combo::RoundRobin>,
    pub(crate) affinity: Arc<crate::routing::affinity::Affinity>,
    pub(crate) cooldown: Arc<crate::routing::cooldown::Cooldown>,
    pub(crate) metrics: Arc<crate::metrics::Metrics>,
    pub(crate) metrics_token: Option<Arc<SecretString>>,
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
        let db = db::connect(&config.database_url).await?;
        let mut crypto = config.crypto;
        db::master_key::load(&db, &mut crypto).await?;
        // A previous process cannot still own a committed stream.
        sqlx::query(
            "UPDATE request_history SET outcome = 'interrupted' WHERE outcome = 'streaming'",
        )
        .execute(&db)
        .await?;
        let history = db::history_writer::HistoryWriter::start(
            db::connect_history_writer(&config.database_url, &db).await?,
        );
        history.set_buffer(db::settings::history_buffer(&db).await?);
        let dashboard_auth = crate::web::auth::DashboardAuth::from_config(
            &db,
            config.dashboard_login,
            config.dashboard_secure_cookie,
        )
        .await?;
        let metrics = Arc::new(crate::metrics::Metrics::default());
        metrics.set_enabled(
            config.metrics_token.is_some()
                || db::settings::metrics_token_hash(&db).await?.is_some(),
        );
        Ok(Self {
            started_at: std::time::Instant::now(),
            history,
            db,
            config: Arc::default(),
            client,
            crypto: Arc::new(crypto),
            api_key: config.api_key.map(Arc::new),
            csrf_token: Arc::new(uuid::Uuid::new_v4().to_string()),
            dashboard_auth,
            key_limits: Arc::default(),
            spending: Arc::default(),
            round_robin: Arc::default(),
            affinity: Arc::default(),
            cooldown: Arc::default(),
            metrics,
            metrics_token: config.metrics_token.map(Arc::new),
        })
    }

    /// The one-time code for first-run dashboard setup, while no password exists.
    pub fn dashboard_setup_code(&self) -> Option<String> {
        self.dashboard_auth
            .as_ref()
            .and_then(|auth| auth.setup_code())
    }

    /// Wait until request history queued so far is committed or dropped.
    pub async fn flush_history(&self) {
        self.history.flush().await;
    }
}

pub fn router(state: AppState) -> Router {
    let authenticated = |router: Router<AppState>| {
        router.layer(middleware::from_fn_with_state(state.clone(), authenticate))
    };
    // Anthropic clients read errors in Anthropic's shape, authentication included.
    let api = authenticated(crate::api::router()).merge(
        authenticated(crate::api::anthropic_router())
            .layer(middleware::from_fn(crate::api::anthropic_errors)),
    );
    let dashboard = crate::web::router()
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::web::auth::authenticate,
        ))
        .layer(middleware::from_fn(dashboard_origin));
    Router::new()
        .nest("/v1", api)
        .merge(dashboard)
        .merge(crate::health::router())
        .merge(crate::metrics::router())
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
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let access = authorize(&state, request.headers())
        .await
        .inspect_err(|error| {
            if matches!(error, AppError::Unauthorized) {
                state.metrics.rejected("unauthorized");
            }
        })?;
    request.extensions_mut().insert(access.access.clone());
    request.extensions_mut().insert(access);
    Ok(next.run(request).await)
}

#[derive(Clone)]
pub(crate) struct Authorization {
    pub access: crate::db::api_keys::KeyAccess,
    pub key: Option<crate::db::api_keys::AuthenticatedKey>,
}
impl Authorization {
    /// Shared environment-key/open access each has one namespace; generated
    /// keys never share placement hints with another caller.
    pub(crate) fn affinity_scope(&self, state: &AppState) -> String {
        match &self.key {
            Some(key) => format!("key:{}", key.id),
            None if state.api_key.is_some() => "environment".into(),
            None => "anonymous".into(),
        }
    }

    /// Check the key's budget, then reserve its traffic limits. A refused
    /// budget does not consume rate quota.
    pub(crate) async fn admit(&self, state: &AppState) -> Result<Admission, AppError> {
        let Some(key) = &self.key else {
            return Ok(Admission::default());
        };
        let charge = match key.budget {
            Some(budget) => {
                let admitted = state
                    .spending
                    .admit(&state.db, &key.id, budget, unix_now())
                    .await;
                if matches!(admitted, Err(AppError::BudgetExceeded(_))) {
                    state.metrics.rejected("budget");
                }
                Some(admitted?)
            }
            None => None,
        };
        let permit = state.key_limits.admit(&key.id, key.limits);
        if permit.is_err() {
            state.metrics.rejected("rate_limit");
        }
        Ok(Admission {
            permit: Some(permit?),
            charge,
        })
    }
}

/// What admission reserved for a request: its traffic-limit slot and the
/// budget its reported usage is charged to.
#[derive(Default)]
pub(crate) struct Admission {
    pub permit: Option<crate::key_limits::Permit>,
    pub charge: Option<crate::key_limits::Charge>,
}

/// Shared by the public API and the dashboard's billable chat test action.
pub(crate) async fn authorize(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<Authorization, AppError> {
    let supplied = if headers.contains_key(header::AUTHORIZATION) {
        bearer_token(headers)
    } else {
        // Anthropic SDKs send their key as `x-api-key` instead.
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    };
    if state
        .api_key
        .as_ref()
        .is_some_and(|expected| secret_matches(supplied, expected))
    {
        return Ok(Authorization {
            access: crate::db::api_keys::KeyAccess::All,
            key: None,
        });
    }
    if let Some(key) = authenticate_key(state, supplied).await? {
        return Ok(Authorization {
            access: key.access.clone(),
            key: Some(key),
        });
    }
    // Open only when neither the environment key nor a generated key exists.
    // Expired keys still keep authentication enabled until explicitly revoked.
    if state.api_key.is_none() && !state.config.keys_exist(&state.db).await? {
        return Ok(Authorization {
            access: crate::db::api_keys::KeyAccess::All,
            key: None,
        });
    }
    Err(AppError::Unauthorized)
}

/// The token of an `Authorization: Bearer` header, or empty. The scheme is
/// case-insensitive (RFC 9110); the token is exact.
pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map_or("", |(_, token)| token)
}

/// Constant-time comparison with a configured secret.
pub(crate) fn secret_matches(supplied: &str, expected: &SecretString) -> bool {
    bool::from(
        supplied
            .as_bytes()
            .ct_eq(expected.expose_secret().as_bytes()),
    )
}

/// The system clock in Unix seconds.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
        .try_into()
        .unwrap_or(i64::MAX)
}

/// [`crate::db::api_keys::authenticate_key`] through the configuration cache.
async fn authenticate_key(
    state: &AppState,
    secret: &str,
) -> Result<Option<crate::db::api_keys::AuthenticatedKey>, AppError> {
    if secret.is_empty() || secret.len() > 256 {
        return Ok(None);
    }
    let fingerprint = state.crypto.gateway_key_fingerprint(secret)?;
    state.config.key(&state.db, fingerprint, unix_now()).await
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
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'"));
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}
