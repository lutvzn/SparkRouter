//! Optional, single-admin dashboard login. Sessions are bounded, process-local,
//! expire after eight hours, and are invalidated by logout or restart.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};

use aes_gcm::aead::{OsRng, rand_core::RngCore};
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
use subtle::ConstantTimeEq;

use crate::{app::AppState, error::AppError};

const COOKIE: &str = "sparkrouter_session";
const LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);

pub struct DashboardAuth {
    password: SecretString,
    secure_cookie: bool,
    inner: Mutex<Sessions>,
}

struct Sessions {
    tokens: HashMap<String, Instant>,
    failures: HashMap<IpAddr, FailureWindow>,
}

struct FailureWindow {
    started: Instant,
    count: usize,
}

impl DashboardAuth {
    pub fn new(password: SecretString, secure_cookie: bool) -> Self {
        Self {
            password,
            secure_cookie,
            inner: Mutex::new(Sessions {
                tokens: HashMap::new(),
                failures: HashMap::new(),
            }),
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
    if path == "/login" || path.starts_with("/static/") || auth.valid(request.headers()) {
        return next.run(request).await;
    }
    if matches!(*request.method(), Method::GET | Method::HEAD)
        && !request.headers().contains_key("hx-request")
    {
        super::see_other("/login")
    } else {
        (StatusCode::UNAUTHORIZED, "Dashboard login required").into_response()
    }
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    app_name: &'static str,
    csrf_token: String,
    error: &'static str,
}

pub async fn page(State(state): State<AppState>) -> Result<Response, AppError> {
    if state.dashboard_auth.is_none() {
        return Ok(super::see_other("/"));
    }
    Ok(super::render(LoginPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.to_string(),
        error: "",
    })?
    .into_response())
}

#[derive(Deserialize)]
pub struct LoginForm {
    _csrf: String,
    #[serde(deserialize_with = "deserialize_password")]
    password: SecretString,
}

fn deserialize_password<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SecretString, D::Error> {
    String::deserialize(deserializer).map(SecretString::from)
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
    let now = Instant::now();
    // Trust the actual socket peer, never attacker-supplied forwarding headers.
    // Reverse proxies sharing one peer should add their own per-client limit.
    let peer = peer
        .map(|Extension(ConnectInfo(address))| address.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let token = {
        let mut sessions = auth.inner.lock().map_err(|_| AppError::Provider)?;
        sessions
            .failures
            .retain(|_, window| now.duration_since(window.started) < Duration::from_secs(60));
        let bucket = failure_bucket(&mut sessions.failures, peer);
        if sessions
            .failures
            .get(&bucket)
            .is_some_and(|window| window.count >= MAX_FAILURES)
        {
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "60")],
                "Too many login attempts; try again in a minute",
            )
                .into_response());
        }
        // Fingerprint both values before constant-time comparison so password
        // length/prefix are not revealed by the comparison itself.
        let supplied = state
            .crypto
            .dashboard_password_fingerprint(form.password.expose_secret())?;
        let expected = state
            .crypto
            .dashboard_password_fingerprint(auth.password.expose_secret())?;
        if !bool::from(supplied.as_bytes().ct_eq(expected.as_bytes())) {
            sessions
                .failures
                .entry(bucket)
                .or_insert(FailureWindow {
                    started: now,
                    count: 0,
                })
                .count += 1;
            drop(sessions);
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
        // A successful login clears that peer's own failures, never the shared overflow bucket.
        if bucket == peer {
            sessions.failures.remove(&peer);
        }
        sessions.tokens.retain(|_, expires| *expires > now);
        // Do not grow without bound or silently evict an active session.
        if sessions.tokens.len() >= 128 {
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                "Too many active dashboard sessions",
            )
                .into_response());
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let token = URL_SAFE_NO_PAD.encode(random);
        sessions.tokens.insert(token.clone(), now + LIFETIME);
        token
    };
    let mut response = super::see_other("/");
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth.cookie_header(&token, LIFETIME.as_secs()),
    );
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
        let auth = DashboardAuth::new(SecretString::from("long-test-password-for-admin"), false);
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
}
