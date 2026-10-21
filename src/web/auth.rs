//! Single-admin dashboard login, required by default even on loopback. The
//! password is an Argon2id hash in SQLite chosen at first-run setup, or
//! `ROUTER_DASHBOARD_PASSWORD`. Setup needs a one-time code printed at startup,
//! so whoever reaches an unconfigured dashboard first cannot claim it. Sessions
//! are bounded, process-local, expire after eight hours, and are invalidated by
//! logout, a password change, or restart.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use aes_gcm::aead::{OsRng, rand_core::RngCore};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use askama::Template;
use axum::{
    Extension, Form,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use crate::{app::AppState, config::DashboardLogin, crypto::Crypto, error::AppError};

const COOKIE: &str = "sparkrouter_session";
const LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);
const MAX_SESSIONS: usize = 128;
const MIN_PASSWORD_CHARS: usize = 12;
const MAX_PASSWORD_BYTES: usize = 1024;

pub struct DashboardAuth {
    environment: Option<SecretString>,
    secure_cookie: bool,
    inner: Mutex<Sessions>,
    /// Each Argon2 computation uses 19 MiB; bound how many run at once.
    hashing: Semaphore,
}

struct Sessions {
    tokens: HashMap<String, Instant>,
    failures: HashMap<IpAddr, FailureWindow>,
    /// Argon2id PHC string of the stored password; unused with `environment`.
    stored: Option<String>,
    /// Authorizes first-run setup; present only while no password exists.
    setup_code: Option<String>,
}

struct FailureWindow {
    started: Instant,
    count: usize,
}

impl DashboardAuth {
    fn build(
        environment: Option<SecretString>,
        stored: Option<String>,
        secure_cookie: bool,
    ) -> Self {
        let setup_code = (environment.is_none() && stored.is_none()).then(new_setup_code);
        Self {
            environment,
            secure_cookie,
            inner: Mutex::new(Sessions {
                tokens: HashMap::new(),
                failures: HashMap::new(),
                stored,
                setup_code,
            }),
            hashing: Semaphore::new(2),
        }
    }

    /// None when login is disabled. A stored password is read once here, so
    /// `sparkrouter reset-dashboard-password` applies on the next start.
    pub(crate) async fn from_config(
        db: &SqlitePool,
        login: DashboardLogin,
        secure_cookie: bool,
    ) -> Result<Option<Arc<Self>>, AppError> {
        let auth = match login {
            DashboardLogin::Disabled => return Ok(None),
            DashboardLogin::Environment(password) => {
                Self::build(Some(password), None, secure_cookie)
            }
            DashboardLogin::Stored => Self::build(
                None,
                crate::db::dashboard_admin::password_hash(db).await?,
                secure_cookie,
            ),
        };
        Ok(Some(Arc::new(auth)))
    }

    /// The one-time code that authorizes first-run setup, while it is needed.
    pub fn setup_code(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|sessions| sessions.setup_code.clone())
    }

    fn needs_setup(&self) -> bool {
        self.inner
            .lock()
            .map_or(true, |sessions| sessions.setup_code.is_some())
    }

    pub(crate) fn changeable(&self) -> bool {
        self.environment.is_none()
    }

    pub(crate) fn describe(&self) -> &'static str {
        if self.environment.is_some() {
            "Required — password from ROUTER_DASHBOARD_PASSWORD"
        } else if self.needs_setup() {
            "Required — waiting for first-run setup"
        } else {
            "Required — stored password (Argon2id hash)"
        }
    }

    fn valid(&self, headers: &HeaderMap) -> bool {
        let Some(token) = cookie(headers) else {
            return false;
        };
        self.inner.lock().is_ok_and(|sessions| {
            sessions
                .tokens
                .get(token)
                .is_some_and(|expires| *expires > Instant::now())
        })
    }

    fn cookie_header(&self, token: &str, max_age: u64) -> HeaderValue {
        HeaderValue::from_str(&format!(
            "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
            if self.secure_cookie { "; Secure" } else { "" }
        ))
        .expect("generated cookie contains only safe ASCII")
    }

    /// Count an attempt against its bucket before checking it, so concurrent
    /// guesses cannot exceed the limit while a slow hash runs. None when the
    /// bucket is throttled.
    fn begin_attempt(&self, peer: IpAddr) -> Result<Option<IpAddr>, AppError> {
        let now = Instant::now();
        let mut sessions = self.inner.lock().map_err(|_| AppError::Provider)?;
        sessions
            .failures
            .retain(|_, window| now.duration_since(window.started) < Duration::from_secs(60));
        let bucket = failure_bucket(&mut sessions.failures, peer);
        let window = sessions.failures.entry(bucket).or_insert(FailureWindow {
            started: now,
            count: 0,
        });
        if window.count >= MAX_FAILURES {
            return Ok(None);
        }
        window.count += 1;
        Ok(Some(bucket))
    }

    /// A successful check clears that peer's own failures, and only removes
    /// its own attempt from the shared overflow bucket.
    fn succeeded(&self, sessions: &mut Sessions, bucket: IpAddr, peer: IpAddr) {
        if bucket == peer {
            sessions.failures.remove(&peer);
        } else if let Some(window) = sessions.failures.get_mut(&bucket) {
            window.count = window.count.saturating_sub(1);
        }
    }

    /// Do not grow without bound or silently evict an active session.
    fn start_session(sessions: &mut Sessions) -> Option<String> {
        let now = Instant::now();
        sessions.tokens.retain(|_, expires| *expires > now);
        if sessions.tokens.len() >= MAX_SESSIONS {
            return None;
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let token = URL_SAFE_NO_PAD.encode(random);
        sessions.tokens.insert(token.clone(), now + LIFETIME);
        Some(token)
    }

    async fn verify(&self, crypto: &Crypto, password: SecretString) -> Result<bool, AppError> {
        if let Some(expected) = &self.environment {
            // Fingerprint both values before constant-time comparison so password
            // length/prefix are not revealed by the comparison itself.
            let supplied = crypto.dashboard_password_fingerprint(password.expose_secret())?;
            let expected = crypto.dashboard_password_fingerprint(expected.expose_secret())?;
            return Ok(bool::from(supplied.as_bytes().ct_eq(expected.as_bytes())));
        }
        let stored = self
            .inner
            .lock()
            .map_err(|_| AppError::Provider)?
            .stored
            .clone();
        let Some(stored) = stored else {
            return Ok(false);
        };
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|_| AppError::Provider)?;
        tokio::task::spawn_blocking(move || {
            Argon2::default()
                .verify_password(password.expose_secret().as_bytes(), stored.as_str())
                .is_ok()
        })
        .await
        .map_err(|_| AppError::Provider)
    }

    async fn hash(&self, password: SecretString) -> Result<String, AppError> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|_| AppError::Provider)?;
        tokio::task::spawn_blocking(move || {
            Argon2::default()
                .hash_password(password.expose_secret().as_bytes())
                .map(|hash| hash.to_string())
        })
        .await
        .map_err(|_| AppError::Provider)?
        .map_err(|_| AppError::Encryption)
    }

    fn setup_code_matches(&self, supplied: &str) -> bool {
        let normalize = |code: &str| {
            code.chars()
                .filter(|c| *c != '-' && !c.is_whitespace())
                .map(|c| c.to_ascii_uppercase())
                .collect::<String>()
        };
        let Some(expected) = self.setup_code() else {
            return false;
        };
        let (supplied, expected) = (normalize(supplied), normalize(&expected));
        supplied.len() == expected.len()
            && bool::from(supplied.as_bytes().ct_eq(expected.as_bytes()))
    }
}

/// 16 characters from a 32-symbol alphabet without look-alikes: 80 random bits.
fn new_setup_code() -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut random = [0u8; 16];
    OsRng.fill_bytes(&mut random);
    random
        .chunks(4)
        .map(|chunk| {
            chunk
                .iter()
                .map(|byte| char::from(ALPHABET[usize::from(byte % 32)]))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == COOKIE && value.len() == 43).then_some(value)
        })
}

pub async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(auth) = &state.dashboard_auth else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    if path.starts_with("/static/") {
        return next.run(request).await;
    }
    let (open, destination, message) = if auth.needs_setup() {
        (path == "/setup", "/setup", "Dashboard setup required")
    } else {
        (
            path == "/login" || auth.valid(request.headers()),
            "/login",
            "Dashboard login required",
        )
    };
    if open {
        return next.run(request).await;
    }
    if matches!(*request.method(), Method::GET | Method::HEAD)
        && !request.headers().contains_key("hx-request")
    {
        super::see_other(destination)
    } else {
        (StatusCode::UNAUTHORIZED, message).into_response()
    }
}

fn peer_address(peer: Option<Extension<ConnectInfo<SocketAddr>>>) -> IpAddr {
    // Trust the actual socket peer, never attacker-supplied forwarding headers.
    // Reverse proxies sharing one peer should add their own per-client limit.
    peer.map(|Extension(ConnectInfo(address))| address.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

fn throttled() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        "Too many attempts; try again in a minute",
    )
        .into_response()
}

fn too_many_sessions() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        "Too many active dashboard sessions",
    )
        .into_response()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasswordProblem {
    Mismatch,
    Short,
    Long,
    Blank,
}

impl PasswordProblem {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Mismatch => "The new passwords do not match.",
            Self::Short => "Use at least 12 characters for the dashboard password.",
            Self::Long => "Use at most 1024 bytes for the dashboard password.",
            Self::Blank => "The dashboard password must not be blank.",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Mismatch => "password-mismatch",
            Self::Short => "password-short",
            Self::Long => "password-long",
            Self::Blank => "password-blank",
        }
    }

    pub(crate) fn from_code(code: &str) -> Option<Self> {
        [Self::Mismatch, Self::Short, Self::Long, Self::Blank]
            .into_iter()
            .find(|problem| problem.code() == code)
    }
}

fn check_new_password(
    password: &SecretString,
    confirm: &SecretString,
) -> Result<(), PasswordProblem> {
    let (password, confirm) = (password.expose_secret(), confirm.expose_secret());
    if !bool::from(password.as_bytes().ct_eq(confirm.as_bytes())) {
        Err(PasswordProblem::Mismatch)
    } else if password.trim().is_empty() {
        Err(PasswordProblem::Blank)
    } else if password.chars().count() < MIN_PASSWORD_CHARS {
        Err(PasswordProblem::Short)
    } else if password.len() > MAX_PASSWORD_BYTES {
        Err(PasswordProblem::Long)
    } else {
        Ok(())
    }
}

fn deserialize_password<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SecretString, D::Error> {
    String::deserialize(deserializer).map(SecretString::from)
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    app_name: &'static str,
    csrf_token: String,
    error: &'static str,
}

pub async fn page(State(state): State<AppState>) -> Result<Response, AppError> {
    match &state.dashboard_auth {
        None => Ok(super::see_other("/")),
        Some(auth) if auth.needs_setup() => Ok(super::see_other("/setup")),
        Some(_) => Ok(super::render(LoginPage {
            app_name: crate::APP_NAME,
            csrf_token: state.csrf_token.to_string(),
            error: "",
        })?
        .into_response()),
    }
}

#[derive(Deserialize)]
pub struct LoginForm {
    _csrf: String,
    #[serde(deserialize_with = "deserialize_password")]
    password: SecretString,
}

pub async fn login(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    super::check_csrf(&state, &form._csrf)?;
    let Some(auth) = &state.dashboard_auth else {
        return Ok(super::see_other("/"));
    };
    if auth.needs_setup() {
        return Ok(super::see_other("/setup"));
    }
    let peer = peer_address(peer);
    let Some(bucket) = auth.begin_attempt(peer)? else {
        return Ok(throttled());
    };
    if !auth.verify(&state.crypto, form.password).await? {
        return Ok((
            StatusCode::UNAUTHORIZED,
            super::render(LoginPage {
                app_name: crate::APP_NAME,
                csrf_token: state.csrf_token.to_string(),
                error: "Incorrect password",
            })?,
        )
            .into_response());
    }
    let token = {
        let mut sessions = auth.inner.lock().map_err(|_| AppError::Provider)?;
        auth.succeeded(&mut sessions, bucket, peer);
        DashboardAuth::start_session(&mut sessions)
    };
    let Some(token) = token else {
        return Ok(too_many_sessions());
    };
    let mut response = super::see_other("/");
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth.cookie_header(&token, LIFETIME.as_secs()),
    );
    Ok(response)
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    app_name: &'static str,
    csrf_token: String,
    error: &'static str,
}

pub async fn setup_page(State(state): State<AppState>) -> Result<Response, AppError> {
    match &state.dashboard_auth {
        None => Ok(super::see_other("/")),
        Some(auth) if !auth.needs_setup() => Ok(super::see_other("/login")),
        Some(_) => Ok(super::render(SetupPage {
            app_name: crate::APP_NAME,
            csrf_token: state.csrf_token.to_string(),
            error: "",
        })?
        .into_response()),
    }
}

#[derive(Deserialize)]
pub struct SetupForm {
    _csrf: String,
    setup_code: String,
    #[serde(deserialize_with = "deserialize_password")]
    password: SecretString,
    #[serde(deserialize_with = "deserialize_password")]
    confirm: SecretString,
}

pub async fn setup(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Form(form): Form<SetupForm>,
) -> Result<Response, AppError> {
    super::check_csrf(&state, &form._csrf)?;
    let Some(auth) = &state.dashboard_auth else {
        return Ok(super::see_other("/"));
    };
    if !auth.needs_setup() {
        return Ok(super::see_other("/login"));
    }
    let rejected = |status: StatusCode, error: &'static str| -> Result<Response, AppError> {
        Ok((
            status,
            super::render(SetupPage {
                app_name: crate::APP_NAME,
                csrf_token: state.csrf_token.to_string(),
                error,
            })?,
        )
            .into_response())
    };
    let peer = peer_address(peer);
    let Some(bucket) = auth.begin_attempt(peer)? else {
        return Ok(throttled());
    };
    if !auth.setup_code_matches(&form.setup_code) {
        return rejected(StatusCode::UNAUTHORIZED, "Incorrect setup code");
    }
    // A correct code is not a failed attempt, even if the password is rejected.
    auth.succeeded(
        &mut *auth.inner.lock().map_err(|_| AppError::Provider)?,
        bucket,
        peer,
    );
    if let Err(problem) = check_new_password(&form.password, &form.confirm) {
        return rejected(StatusCode::BAD_REQUEST, problem.message());
    }
    let hash = auth.hash(form.password).await?;
    if !crate::db::dashboard_admin::create(&state.db, &hash).await? {
        // A concurrent setup stored its password first.
        return Ok(super::see_other("/login"));
    }
    let token = {
        let mut sessions = auth.inner.lock().map_err(|_| AppError::Provider)?;
        sessions.stored = Some(hash);
        sessions.setup_code = None;
        DashboardAuth::start_session(&mut sessions)
    };
    tracing::info!("dashboard password set; first-run setup complete");
    let Some(token) = token else {
        return Ok(super::see_other("/login"));
    };
    let mut response = super::see_other("/");
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth.cookie_header(&token, LIFETIME.as_secs()),
    );
    Ok(response)
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    _csrf: String,
    #[serde(deserialize_with = "deserialize_password")]
    current: SecretString,
    #[serde(deserialize_with = "deserialize_password")]
    password: SecretString,
    #[serde(deserialize_with = "deserialize_password")]
    confirm: SecretString,
}

/// Replaces the stored password and signs out every other session.
pub async fn change_password(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Form(form): Form<ChangePasswordForm>,
) -> Result<Response, AppError> {
    super::check_csrf(&state, &form._csrf)?;
    let auth = match &state.dashboard_auth {
        Some(auth) if auth.changeable() => auth,
        Some(_) => {
            return Err(AppError::InvalidRequest(
                "the dashboard password comes from ROUTER_DASHBOARD_PASSWORD; change it there and restart"
                    .into(),
            ));
        }
        None => {
            return Err(AppError::InvalidRequest(
                "dashboard login is disabled by ROUTER_DASHBOARD_AUTH".into(),
            ));
        }
    };
    let peer = peer_address(peer);
    let Some(bucket) = auth.begin_attempt(peer)? else {
        return Ok(throttled());
    };
    if !auth.verify(&state.crypto, form.current).await? {
        return Ok(super::see_other("/settings?result=password-wrong"));
    }
    auth.succeeded(
        &mut *auth.inner.lock().map_err(|_| AppError::Provider)?,
        bucket,
        peer,
    );
    if let Err(problem) = check_new_password(&form.password, &form.confirm) {
        return Ok(super::see_other(&format!(
            "/settings?result={}",
            problem.code()
        )));
    }
    let hash = auth.hash(form.password).await?;
    crate::db::dashboard_admin::replace(&state.db, &hash).await?;
    let token = {
        let mut sessions = auth.inner.lock().map_err(|_| AppError::Provider)?;
        sessions.stored = Some(hash);
        sessions.tokens.clear();
        DashboardAuth::start_session(&mut sessions)
    };
    tracing::info!("dashboard password changed; other sessions revoked");
    let mut response = super::see_other("/settings?result=password");
    if let Some(token) = token {
        response.headers_mut().insert(
            header::SET_COOKIE,
            auth.cookie_header(&token, LIFETIME.as_secs()),
        );
    }
    Ok(response)
}

const MAX_TRACKED_PEERS: usize = 1024;
const MAX_FAILURES: usize = 10;

/// Bound accounting without evicting a throttled peer. When the table is full,
/// make room by dropping the oldest peer that is not throttled, so a burst of
/// sources cannot push new legitimate peers into one shared lockout. Only when
/// every tracked peer is throttled do new peers share the overflow bucket.
fn failure_bucket(failures: &mut HashMap<IpAddr, FailureWindow>, peer: IpAddr) -> IpAddr {
    if failures.contains_key(&peer) || failures.len() < MAX_TRACKED_PEERS {
        return peer;
    }
    let evictable = failures
        .iter()
        .filter(|(_, window)| window.count < MAX_FAILURES)
        .min_by_key(|(_, window)| window.started)
        .map(|(address, _)| *address);
    match evictable {
        Some(address) => {
            failures.remove(&address);
            peer
        }
        None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    }
}

#[derive(Template)]
#[template(path = "logout.html")]
struct LogoutPage {
    app_name: &'static str,
    csrf_token: String,
}

pub async fn logout_page(State(state): State<AppState>) -> Result<Response, AppError> {
    if state.dashboard_auth.is_none() {
        return Ok(super::see_other("/"));
    }
    Ok(super::render(LogoutPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.to_string(),
    })?
    .into_response())
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<super::CsrfForm>,
) -> Result<Response, AppError> {
    super::check_csrf(&state, &form._csrf)?;
    let mut response = super::see_other("/login");
    if let Some(auth) = &state.dashboard_auth {
        if let Some(token) = cookie(&headers) {
            auth.inner
                .lock()
                .map_err(|_| AppError::Provider)?
                .tokens
                .remove(token);
        }
        response
            .headers_mut()
            .insert(header::SET_COOKIE, auth.cookie_header("", 0));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_session_fails_closed() {
        let auth = DashboardAuth::build(
            Some(SecretString::from("long-test-password-for-admin")),
            None,
            false,
        );
        let token = URL_SAFE_NO_PAD.encode([1u8; 32]);
        auth.inner
            .lock()
            .unwrap()
            .tokens
            .insert(token.clone(), Instant::now() - Duration::from_secs(1));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{COOKIE}={token}")).unwrap(),
        );
        assert!(!auth.valid(&headers));
        auth.inner
            .lock()
            .unwrap()
            .tokens
            .insert(token, Instant::now() + LIFETIME);
        assert!(auth.valid(&headers));
    }

    #[test]
    fn full_failure_table_evicts_unthrottled_peers_before_sharing_a_bucket() {
        let base = Instant::now();
        let address =
            |index: usize| IpAddr::from([10, 0, (index / 256) as u8, (index % 256) as u8]);
        let mut failures = HashMap::new();
        for index in 0..MAX_TRACKED_PEERS {
            failures.insert(
                address(index),
                FailureWindow {
                    started: base + Duration::from_millis(index as u64),
                    count: if index == 0 { MAX_FAILURES } else { 1 },
                },
            );
        }
        let newcomer = IpAddr::from([192, 168, 0, 1]);
        // The oldest unthrottled peer makes room; the throttled one is kept.
        assert_eq!(failure_bucket(&mut failures, newcomer), newcomer);
        assert!(failures.contains_key(&address(0)));
        assert!(!failures.contains_key(&address(1)));
        // Tracked peers always keep their own bucket.
        assert_eq!(failure_bucket(&mut failures, address(0)), address(0));
        // Only when every tracked peer is throttled do newcomers share one.
        for window in failures.values_mut() {
            window.count = MAX_FAILURES;
        }
        failures.insert(
            newcomer,
            FailureWindow {
                started: base,
                count: MAX_FAILURES,
            },
        );
        assert_eq!(failures.len(), MAX_TRACKED_PEERS);
        assert_eq!(
            failure_bucket(&mut failures, IpAddr::from([192, 168, 0, 2])),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn malformed_and_unrelated_cookies_are_not_sessions() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=value; sparkrouter_session=short"),
        );
        assert!(cookie(&headers).is_none());
    }

    #[test]
    fn setup_codes_are_random_unambiguous_and_matched_loosely_but_exactly() {
        let code = new_setup_code();
        assert_eq!(code.len(), 19);
        assert!(
            code.chars()
                .all(|c| c == '-' || "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(c))
        );
        assert_ne!(code, new_setup_code());
        let auth = DashboardAuth::build(None, None, false);
        let code = auth.setup_code().unwrap();
        assert!(auth.setup_code_matches(&code));
        assert!(auth.setup_code_matches(&format!(" {} ", code.to_lowercase().replace('-', ""))));
        assert!(!auth.setup_code_matches(&code[..18]));
        assert!(!auth.setup_code_matches(""));
        // Environment and stored passwords never offer setup.
        assert!(
            DashboardAuth::build(Some(SecretString::from("x".repeat(16))), None, false)
                .setup_code()
                .is_none()
        );
        assert!(
            DashboardAuth::build(None, Some("$argon2id$v=19$x".into()), false)
                .setup_code()
                .is_none()
        );
    }

    #[test]
    fn new_passwords_need_confirmation_and_bounded_length() {
        let check = |password: &str, confirm: &str| {
            check_new_password(&SecretString::from(password), &SecretString::from(confirm))
        };
        assert_eq!(check("twelve chars", "twelve chars"), Ok(()));
        assert_eq!(
            check("twelve chars", "twelve chars!"),
            Err(PasswordProblem::Mismatch)
        );
        assert_eq!(
            check("eleven char", "eleven char"),
            Err(PasswordProblem::Short)
        );
        assert_eq!(
            check(&" ".repeat(20), &" ".repeat(20)),
            Err(PasswordProblem::Blank)
        );
        let long = "x".repeat(1025);
        assert_eq!(check(&long, &long), Err(PasswordProblem::Long));
        // Characters, not bytes, count toward the minimum.
        assert_eq!(check("ääääääääääää", "ääääääääääää"), Ok(()));
        for problem in [
            PasswordProblem::Mismatch,
            PasswordProblem::Short,
            PasswordProblem::Long,
            PasswordProblem::Blank,
        ] {
            assert_eq!(PasswordProblem::from_code(problem.code()), Some(problem));
        }
    }

    #[tokio::test]
    async fn stored_passwords_are_argon2id_and_verify_only_the_original() {
        let crypto = Crypto::from_base64(&SecretString::from(
            base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
        ))
        .unwrap();
        let auth = DashboardAuth::build(None, None, false);
        let hash = auth
            .hash(SecretString::from("stored admin password"))
            .await
            .unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains("stored admin password"));
        auth.inner.lock().unwrap().stored = Some(hash);
        assert!(
            auth.verify(&crypto, SecretString::from("stored admin password"))
                .await
                .unwrap()
        );
        assert!(
            !auth
                .verify(&crypto, SecretString::from("stored admin passwore"))
                .await
                .unwrap()
        );
    }
}
