//! Integration tests for the single-admin dashboard login (`src/web/auth.rs`),
//! covering the whole login lifecycle through the real `app::router`: the
//! default stored password with first-run setup (required even on loopback),
//! open mode only when explicitly disabled, redirects and
//! 401s for browser vs HTMX/unsafe requests, login CSRF and origin checks,
//! session cookie attributes, forged-cookie rejection, logout revocation,
//! login rate limiting, credential non-disclosure, gateway-API independence,
//! and restart invalidation. No upstream provider is contacted anywhere.

use axum::{
    Router,
    body::Body,
    extract::connect_info::ConnectInfo,
    http::{Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::SecretString;
use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db,
};
use std::net::SocketAddr;
use tower::ServiceExt;

/// The configured dashboard password (kept at the 16+ byte `from_env` minimum
/// even though these tests build `Config` directly).
const PASSWORD: &str = "correct-dashboard-password";
const COOKIE_NAME: &str = "sparkrouter_session";

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

fn auth_config(password: Option<&str>, secure_cookie: bool, api_key: Option<&str>) -> Config {
    Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([61; 32]))).unwrap(),
        api_key: api_key.map(|key| SecretString::from(key.to_owned())),
        dashboard_login: match password {
            Some(password) => DashboardLogin::Environment(SecretString::from(password.to_owned())),
            None => DashboardLogin::Disabled,
        },
        dashboard_secure_cookie: secure_cookie,
        metrics_token: None,
    }
}

async fn auth_state(
    password: Option<&str>,
    secure_cookie: bool,
    api_key: Option<&str>,
) -> AppState {
    AppState::from_config(auth_config(password, secure_cookie, api_key))
        .await
        .unwrap()
}

/// URL-encode form pairs the way the dashboard test helpers do.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").unwrap();
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap().to_owned()
}

async fn post_form(
    router: &Router,
    path: &str,
    pairs: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> Response {
    let mut builder = Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("host", "localhost");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(builder.body(Body::from(form_body(pairs))).unwrap())
        .await
        .unwrap()
}

async fn login(router: &Router, state: &AppState, password: &str) -> Response {
    post_form(
        router,
        "/login",
        &[("_csrf", state.csrf_token.as_str()), ("password", password)],
        &[],
    )
    .await
}

/// Log in as a concrete socket peer (the `ConnectInfo` the server injects via
/// `into_make_service_with_connect_info`), optionally with extra headers such
/// as spoofed forwarding headers that must be ignored for throttling.
async fn login_from_peer(
    router: &Router,
    state: &AppState,
    password: &str,
    peer: Option<SocketAddr>,
    headers: &[(&str, &str)],
) -> Response {
    let mut builder = Request::post("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("host", "localhost");
    if let Some(peer) = peer {
        builder = builder.extension(ConnectInfo(peer));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(
            builder
                .body(Body::from(form_body(&[
                    ("_csrf", state.csrf_token.as_str()),
                    ("password", password),
                ])))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get(router: &Router, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut builder = Request::get(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn location(response: &Response) -> &str {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("redirect carries a Location header")
}

/// The `sparkrouter_session` Set-Cookie header, if any was sent.
fn session_cookie(response: &Response) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("sparkrouter_session="))
        .map(str::to_owned)
}

/// The raw token inside a session Set-Cookie header.
fn token_of(set_cookie: &str) -> &str {
    set_cookie
        .strip_prefix("sparkrouter_session=")
        .expect("session cookie carries its name")
        .split(';')
        .next()
        .unwrap()
}

fn cookie_header(token: &str) -> String {
    format!("{COOKIE_NAME}={token}")
}

/// 32 random bytes in URL-safe unpadded base64 are exactly 43 characters.
fn is_43_char_base64url(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

// ---------------------------------------------------------------------------
// Open mode: login explicitly disabled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_login_disabled_the_dashboard_stays_open() {
    let state = auth_state(None, false, None).await;
    let router = app::router(state.clone());

    // The dashboard home renders directly without any cookie.
    let index = get(&router, "/", &[]).await;
    assert_eq!(index.status(), StatusCode::OK);
    for path in ["/providers", "/combos", "/api-keys"] {
        let response = get(&router, path, &[]).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "GET {path} must stay open"
        );
        assert!(session_cookie(&response).is_none());
    }

    // The login and logout endpoints degrade to plain redirects: there is no
    // session to create, clear, or confirm.
    let page = get(&router, "/login", &[]).await;
    assert_eq!(page.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&page), "/");

    let posted = login(&router, &state, "anything-at-all").await;
    assert_eq!(posted.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&posted), "/");
    assert!(
        session_cookie(&posted).is_none(),
        "no session cookie without a password"
    );

    let logout = get(&router, "/logout", &[]).await;
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&logout), "/");
}

// ---------------------------------------------------------------------------
// Password configured: redirects, 401s, public assets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn password_protected_dashboard_redirects_browsers_and_rejects_htmx_and_unsafe_methods() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());

    // Browser GETs land on the login page with a 303 so the method switches.
    for path in [
        "/",
        "/providers",
        "/providers/new",
        "/providers/catalog/openai",
        "/combos",
        "/api-keys",
        "/history",
        "/settings",
        "/test",
        "/logout",
    ] {
        let response = get(&router, path, &[]).await;
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "GET {path} must redirect to the login page"
        );
        assert_eq!(location(&response), "/login");
        assert!(session_cookie(&response).is_none());
    }

    // HTMX GETs get an explicit 401 (no redirect: fetch cannot follow a
    // cross-method redirect usefully) with a stable, script-friendly body.
    let htmx = get(&router, "/providers", &[("hx-request", "true")]).await;
    assert_eq!(htmx.status(), StatusCode::UNAUTHORIZED);
    let html = body(htmx).await;
    assert_eq!(html, "Dashboard login required");

    // Unsafe methods are never redirected either.
    let posted = post_form(
        &router,
        "/combos",
        &[("_csrf", state.csrf_token.as_str()), ("name", "sneaky")],
        &[],
    )
    .await;
    assert_eq!(posted.status(), StatusCode::UNAUTHORIZED);
    assert!(
        db::combos::by_name(&state.db, "sneaky").await.is_err(),
        "an unauthenticated POST must not write rows"
    );

    // The login page itself and static assets stay public so the browser can
    // render the login screen.
    let login_page = get(&router, "/login", &[]).await;
    assert_eq!(login_page.status(), StatusCode::OK);
    let login_html = body(login_page).await;
    assert!(login_html.contains("action=\"/login\""));
    assert!(login_html.contains("name=\"_csrf\""));
    assert!(login_html.contains("name=\"password\""));
    assert!(login_html.contains("type=\"password\""));
    assert!(
        login_html.contains(&format!("value=\"{}\"", state.csrf_token)),
        "the login form embeds the process CSRF token"
    );
    assert!(!login_html.contains(PASSWORD));

    for path in ["/static/app.css", "/static/theme.js", "/static/app.js"] {
        let response = get(&router, path, &[]).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} must stay public");
    }
}

// ---------------------------------------------------------------------------
// Login CSRF and origin enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_rejects_missing_csrf_and_cross_origin_submissions() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());

    // An invalid CSRF token is a 400 that never starts a session.
    let wrong_csrf = post_form(
        &router,
        "/login",
        &[
            ("_csrf", "definitely-not-the-token"),
            ("password", PASSWORD),
        ],
        &[],
    )
    .await;
    assert_eq!(
        wrong_csrf.status(),
        StatusCode::BAD_REQUEST,
        "an invalid CSRF token must be rejected"
    );
    assert!(session_cookie(&wrong_csrf).is_none());
    assert!(!body(wrong_csrf).await.contains(PASSWORD));

    // A missing CSRF field must not log in either, whatever the extractor
    // rejection status is.
    let no_csrf = post_form(&router, "/login", &[("password", PASSWORD)], &[]).await;
    assert!(
        !no_csrf.status().is_success(),
        "a missing CSRF token must be rejected, got {}",
        no_csrf.status()
    );
    assert!(session_cookie(&no_csrf).is_none());

    // Cross-origin submissions are rejected before authentication even with a
    // valid CSRF token and the correct password: a foreign Origin against our
    // Host, and a cross-site fetch annotation.
    for (origin, fetch_site) in [
        (Some("https://attacker.example"), None),
        (None, Some("cross-site")),
    ] {
        let mut headers = vec![("host", "localhost")];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        if let Some(fetch_site) = fetch_site {
            headers.push(("sec-fetch-site", fetch_site));
        }
        let response = post_form(
            &router,
            "/login",
            &[("_csrf", state.csrf_token.as_str()), ("password", PASSWORD)],
            &headers,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "cross-origin login must be rejected (origin={origin:?}, fetch-site={fetch_site:?})"
        );
        assert!(
            session_cookie(&response).is_none(),
            "a rejected cross-origin login must not mint a session"
        );
    }

    // The very same credentials from the dashboard origin succeed, proving the
    // middleware distinguishes origins rather than rejecting all headers.
    let same_origin = post_form(
        &router,
        "/login",
        &[("_csrf", state.csrf_token.as_str()), ("password", PASSWORD)],
        &[("host", "localhost"), ("origin", "http://localhost")],
    )
    .await;
    assert_eq!(same_origin.status(), StatusCode::SEE_OTHER);
    assert!(session_cookie(&same_origin).is_some());
}

// ---------------------------------------------------------------------------
// Wrong password handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wrong_password_is_401_and_never_echoed() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());

    for wrong in [
        "totally-wrong-password",
        "",
        "correct-dashboard-password-with-a-suffix",
        "wrong-<script>alert('xss')</script>",
    ] {
        let response = login(&router, &state, wrong).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "wrong password {wrong:?} must be a 401"
        );
        assert!(session_cookie(&response).is_none());
        let html = body(response).await;
        assert!(
            html.contains("Incorrect password"),
            "error banner for {wrong:?}"
        );
        assert!(
            wrong.is_empty() || !html.contains(wrong),
            "the submitted password must never echo"
        );
        assert!(
            !html.contains(PASSWORD),
            "the real password must never leak"
        );
        assert!(
            html.contains(&format!("value=\"{}\"", state.csrf_token)),
            "the retry form still carries a CSRF token"
        );
    }
}

// ---------------------------------------------------------------------------
// Successful login: cookie shape and access
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_login_sets_a_strict_random_cookie_that_unlocks_the_dashboard() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());

    let first = login(&router, &state, PASSWORD).await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&first), "/");

    let set_cookie = session_cookie(&first).expect("login sets the session cookie");
    let token = token_of(&set_cookie);
    assert!(
        is_43_char_base64url(token),
        "the session token must be 43 URL-safe base64 characters, got {token:?}"
    );
    assert_ne!(token, PASSWORD, "the cookie value is never the password");
    for attribute in [
        " Path=/",
        " HttpOnly",
        " SameSite=Strict",
        // 28800 s = the eight-hour session lifetime. The in-memory expiry
        // itself is Instant-based and cannot be fast-forwarded here, so the
        // advertised Max-Age is the observable contract for it.
        " Max-Age=28800",
    ] {
        assert!(
            set_cookie.contains(attribute),
            "session cookie must carry `{attribute}`: {set_cookie}"
        );
    }
    assert!(
        !set_cookie.contains("Secure"),
        "the Secure flag must be absent unless configured"
    );
    assert!(!set_cookie.contains(PASSWORD));

    // Every login mints a fresh random token.
    let second = login(&router, &state, PASSWORD).await;
    assert_eq!(second.status(), StatusCode::SEE_OTHER);
    let second_set_cookie = session_cookie(&second).expect("second login sets a cookie");
    let second_token = token_of(&second_set_cookie);
    assert_ne!(token, second_token, "session tokens must be random");

    // The valid cookie unlocks every dashboard surface, including HTMX and
    // unsafe requests.
    let cookie = cookie_header(token);
    for path in [
        "/",
        "/providers",
        "/combos",
        "/api-keys",
        "/settings",
        "/history",
        "/test",
    ] {
        let response = get(&router, path, &[("cookie", cookie.as_str())]).await;
        assert_eq!(response.status(), StatusCode::OK, "cookie unlocks {path}");
    }
    let htmx = get(
        &router,
        "/providers",
        &[("cookie", cookie.as_str()), ("hx-request", "true")],
    )
    .await;
    assert_eq!(htmx.status(), StatusCode::OK);

    let created = post_form(
        &router,
        "/combos",
        &[
            ("_csrf", state.csrf_token.as_str()),
            ("name", "after-login"),
        ],
        &[("cookie", cookie.as_str())],
    )
    .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        db::combos::by_name(&state.db, "after-login")
            .await
            .unwrap()
            .name,
        "after-login",
        "an authenticated POST creates rows"
    );

    // Pages rendered with a valid session still never disclose the password.
    let index = body(get(&router, "/providers", &[("cookie", cookie.as_str())]).await).await;
    assert!(!index.contains(PASSWORD));

    // Forged or malformed cookies are treated as anonymous: a well-formed but
    // unknown token, a wrong-length value, and a foreign cookie name.
    for cookie_value in [
        cookie_header(&"A".repeat(43)),
        cookie_header("too-short-token"),
        "other-cookie=value".to_owned(),
    ] {
        let response = get(&router, "/providers", &[("cookie", cookie_value.as_str())]).await;
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "cookie {cookie_value:?} must be rejected"
        );
        assert_eq!(location(&response), "/login");
    }

    // An unknown 43-char token must not authorize unsafe methods either.
    let forged = cookie_header(&"B".repeat(43));
    let forged_post = post_form(
        &router,
        "/combos",
        &[("_csrf", state.csrf_token.as_str()), ("name", "forged")],
        &[("cookie", forged.as_str())],
    )
    .await;
    assert_eq!(forged_post.status(), StatusCode::UNAUTHORIZED);
    assert!(db::combos::by_name(&state.db, "forged").await.is_err());
}

#[tokio::test]
async fn secure_cookie_flag_adds_the_secure_attribute_when_configured() {
    let state = auth_state(Some(PASSWORD), true, None).await;
    let router = app::router(state.clone());

    let response = login(&router, &state, PASSWORD).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let set_cookie = session_cookie(&response).expect("login sets the session cookie");
    assert!(
        set_cookie.contains("; Secure"),
        "the configured secure flag must add the Secure attribute: {set_cookie}"
    );
    for attribute in [" Path=/", " HttpOnly", " SameSite=Strict", " Max-Age=28800"] {
        assert!(
            set_cookie.contains(attribute),
            "the other attributes must survive: {set_cookie}"
        );
    }
}

// ---------------------------------------------------------------------------
// Gateway API independence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gateway_api_and_dashboard_credentials_stay_separate() {
    let state = auth_state(Some(PASSWORD), false, Some("router-env-key")).await;
    let router = app::router(state.clone());

    // The gateway keeps its own authentication: anonymous access is denied
    // because an environment key exists.
    let anonymous = get(&router, "/v1/models", &[]).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    // The environment key still authorizes the gateway while the dashboard
    // stays locked.
    let authed = get(
        &router,
        "/v1/models",
        &[("authorization", "Bearer router-env-key")],
    )
    .await;
    assert_eq!(
        authed.status(),
        StatusCode::OK,
        "the dashboard password must not gate /v1"
    );
    assert!(!body(authed).await.contains("router-env-key"));

    // A dashboard session cookie is not a gateway credential.
    let login_response = login(&router, &state, PASSWORD).await;
    assert_eq!(login_response.status(), StatusCode::SEE_OTHER);
    let token = token_of(&session_cookie(&login_response).expect("login sets a cookie")).to_owned();
    let cookie = cookie_header(&token);
    let cookie_only = get(&router, "/v1/models", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        cookie_only.status(),
        StatusCode::UNAUTHORIZED,
        "a dashboard cookie must never authorize /v1"
    );

    // Conversely, the gateway key is not a dashboard credential.
    let bearer_dashboard = get(
        &router,
        "/providers",
        &[("authorization", "Bearer router-env-key")],
    )
    .await;
    assert_eq!(
        bearer_dashboard.status(),
        StatusCode::SEE_OTHER,
        "the gateway key must never unlock the dashboard"
    );
    assert_eq!(location(&bearer_dashboard), "/login");

    // Without any gateway key configured, the dashboard password alone does
    // not implicitly lock /v1.
    let open_state = auth_state(Some(PASSWORD), false, None).await;
    let open_router = app::router(open_state.clone());
    let response = get(&open_router, "/v1/models", &[]).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the gateway stays open when no gateway key is configured"
    );
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_validates_csrf_and_revokes_the_session_for_replay() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());

    let login_response = login(&router, &state, PASSWORD).await;
    assert_eq!(login_response.status(), StatusCode::SEE_OTHER);
    let token = token_of(&session_cookie(&login_response).expect("login sets a cookie")).to_owned();
    let cookie = cookie_header(&token);

    // The confirmation page renders for a signed-in session and carries the
    // CSRF-protected POST form.
    let page = get(&router, "/logout", &[("cookie", cookie.as_str())]).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = body(page).await;
    assert!(html.contains("action=\"/logout\""));
    assert!(html.contains("name=\"_csrf\""));

    // An invalid CSRF token is a 400 that keeps the session alive.
    let wrong_csrf = post_form(
        &router,
        "/logout",
        &[("_csrf", "definitely-not-the-token")],
        &[("cookie", cookie.as_str())],
    )
    .await;
    assert_eq!(
        wrong_csrf.status(),
        StatusCode::BAD_REQUEST,
        "logout must validate the CSRF token"
    );
    let still_valid = get(&router, "/providers", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        still_valid.status(),
        StatusCode::OK,
        "a rejected logout must not revoke the session"
    );

    // Logout requires an authenticated session even to reach the handler.
    let anonymous = post_form(
        &router,
        "/logout",
        &[("_csrf", state.csrf_token.as_str())],
        &[],
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    // A valid logout redirects to the login page, clears the cookie, and
    // revokes the token server-side: replaying the old cookie is anonymous.
    let logout = post_form(
        &router,
        "/logout",
        &[("_csrf", state.csrf_token.as_str())],
        &[("cookie", cookie.as_str())],
    )
    .await;
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&logout), "/login");
    let clear_cookie = session_cookie(&logout).expect("logout clears the session cookie");
    assert!(
        clear_cookie.starts_with("sparkrouter_session=;"),
        "the cleared cookie empties the value: {clear_cookie}"
    );
    assert!(clear_cookie.contains("Max-Age=0"));

    let replay = get(&router, "/providers", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        replay.status(),
        StatusCode::SEE_OTHER,
        "a logged-out cookie must not authenticate"
    );
    assert_eq!(location(&replay), "/login");
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ten_failed_logins_trigger_a_429_with_retry_after() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());

    // The first ten bad attempts are ordinary 401s.
    for attempt in 1..=10 {
        let response = login(&router, &state, "wrong-password-attempt").await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} must still be a plain 401"
        );
        assert!(session_cookie(&response).is_none());
    }

    // The next attempt is rate limited and carries the retry hint.
    let limited = login(&router, &state, "wrong-password-attempt").await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        limited
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("60"),
        "rate-limited logins must advertise a retry delay"
    );
    let limited_body = body(limited).await;
    assert!(!limited_body.contains(PASSWORD));
    assert!(
        !limited_body.contains("wrong-password-attempt"),
        "the rate-limit response must not echo the attempted password"
    );
}

#[tokio::test]
async fn successful_login_resets_that_peers_failure_count() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());
    let peer: SocketAddr = "192.0.2.30:40003".parse().unwrap();
    for _ in 0..9 {
        let response = login_from_peer(&router, &state, "wrong-password", Some(peer), &[]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let success = login_from_peer(&router, &state, PASSWORD, Some(peer), &[]).await;
    assert_eq!(success.status(), StatusCode::SEE_OTHER);
    // Without the reset, the next failure would be the tenth and the one after throttled.
    for attempt in 1..=10 {
        let response = login_from_peer(&router, &state, "wrong-password", Some(peer), &[]).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} after a success must start a fresh window"
        );
    }
    let throttled = login_from_peer(&router, &state, "wrong-password", Some(peer), &[]).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ---------------------------------------------------------------------------
// Per-peer rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn throttling_is_per_socket_peer_and_forwarded_headers_do_not_bypass_it() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());
    let peer1: SocketAddr = "192.0.2.10:40001".parse().unwrap();
    let peer2: SocketAddr = "192.0.2.20:40002".parse().unwrap();

    // Ten bad attempts from one socket peer exhaust that peer's bucket.
    for attempt in 1..=10 {
        let response = login_from_peer(&router, &state, "wrong-password", Some(peer1), &[]).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "peer1 attempt {attempt} must still be a plain 401"
        );
    }

    // The very next attempt from the same peer is throttled.
    let throttled = login_from_peer(&router, &state, "wrong-password", Some(peer1), &[]).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        throttled
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("60")
    );

    // Spoofed forwarding headers must not re-key or bypass the bucket: the
    // throttle trusts the socket peer, never client-supplied headers.
    for forwarded in ["192.0.2.20", "198.51.100.7, 192.0.2.20"] {
        let response = login_from_peer(
            &router,
            &state,
            "wrong-password",
            Some(peer1),
            &[("x-forwarded-for", forwarded)],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "x-forwarded-for {forwarded:?} must not bypass peer1's throttle"
        );
    }

    // A different socket peer has its own bucket: the correct password still
    // succeeds from peer2 while peer1 is throttled.
    let other_peer = login_from_peer(&router, &state, PASSWORD, Some(peer2), &[]).await;
    assert_eq!(
        other_peer.status(),
        StatusCode::SEE_OTHER,
        "peer2 must not inherit peer1's failures"
    );
    assert!(session_cookie(&other_peer).is_some());
    assert_eq!(location(&other_peer), "/");

    // Peer2's success does not reset peer1's bucket either.
    let still_throttled =
        login_from_peer(&router, &state, "wrong-password", Some(peer1), &[]).await;
    assert_eq!(
        still_throttled.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "peer1 stays throttled after peer2 logs in"
    );
}

// ---------------------------------------------------------------------------
// Restart invalidation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restarting_the_process_invalidates_outstanding_sessions() {
    let first = auth_state(Some(PASSWORD), false, None).await;
    let first_router = app::router(first.clone());
    let login_response = login(&first_router, &first, PASSWORD).await;
    assert_eq!(login_response.status(), StatusCode::SEE_OTHER);
    let token = token_of(&session_cookie(&login_response).expect("login sets a cookie")).to_owned();
    let cookie = cookie_header(&token);

    let authed = get(&first_router, "/providers", &[("cookie", cookie.as_str())]).await;
    assert_eq!(authed.status(), StatusCode::OK);

    // A fresh AppState with the very same password and master key is a
    // restart: sessions are process-local, so the old cookie is worthless.
    let second = auth_state(Some(PASSWORD), false, None).await;
    let second_router = app::router(second.clone());
    let stale = get(&second_router, "/providers", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        stale.status(),
        StatusCode::SEE_OTHER,
        "a session cookie must not survive a restart"
    );
    assert_eq!(location(&stale), "/login");

    // Logging in on the new process works and issues a working session.
    let fresh = login(&second_router, &second, PASSWORD).await;
    assert_eq!(fresh.status(), StatusCode::SEE_OTHER);
    let fresh_set_cookie = session_cookie(&fresh).expect("fresh login sets a cookie");
    let fresh_cookie = cookie_header(token_of(&fresh_set_cookie));
    let authed = get(
        &second_router,
        "/providers",
        &[("cookie", fresh_cookie.as_str())],
    )
    .await;
    assert_eq!(authed.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Default: stored password with first-run setup
// ---------------------------------------------------------------------------

const STORED: &str = "stored admin password";

async fn stored_state(database_url: &str) -> AppState {
    let mut config = auth_config(None, false, None);
    config.database_url = database_url.into();
    config.dashboard_login = DashboardLogin::Stored;
    AppState::from_config(config).await.unwrap()
}

async fn submit_setup(
    router: &Router,
    state: &AppState,
    code: &str,
    password: &str,
    confirm: &str,
) -> Response {
    post_form(
        router,
        "/setup",
        &[
            ("_csrf", state.csrf_token.as_str()),
            ("setup_code", code),
            ("password", password),
            ("confirm", confirm),
        ],
        &[],
    )
    .await
}

fn loopback() -> ConnectInfo<SocketAddr> {
    ConnectInfo("127.0.0.1:50000".parse().unwrap())
}

#[tokio::test]
async fn by_default_even_loopback_requests_must_complete_setup_first() {
    let state = stored_state("sqlite::memory:").await;
    let router = app::router(state.clone());
    assert!(state.dashboard_setup_code().is_some());
    for path in ["/", "/providers", "/settings", "/login", "/logout"] {
        let response = router
            .clone()
            .oneshot(
                Request::get(path)
                    .extension(loopback())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "GET {path}");
        assert_eq!(location(&response), "/setup", "GET {path}");
    }
    let htmx = get(&router, "/providers", &[("hx-request", "true")]).await;
    assert_eq!(htmx.status(), StatusCode::UNAUTHORIZED);
    let unsafe_post = post_form(
        &router,
        "/providers",
        &[("_csrf", state.csrf_token.as_str())],
        &[],
    )
    .await;
    assert_eq!(unsafe_post.status(), StatusCode::UNAUTHORIZED);
    // Logging in is not possible before a password exists.
    let early = login(&router, &state, STORED).await;
    assert_eq!(early.status(), StatusCode::UNAUTHORIZED);
    assert!(session_cookie(&early).is_none());
    let page = get(&router, "/setup", &[]).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = body(page).await;
    assert!(html.contains("Setup code"));
    // The page never shows the code itself.
    assert!(!html.contains(&state.dashboard_setup_code().unwrap()));
    assert_eq!(
        get(&router, "/static/app.css", &[]).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn setup_needs_the_printed_code_a_confirmed_password_and_then_closes() {
    let state = stored_state("sqlite::memory:").await;
    let router = app::router(state.clone());
    let code = state.dashboard_setup_code().unwrap();

    let wrong = submit_setup(&router, &state, "AAAA-AAAA-AAAA-AAAA", STORED, STORED).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(session_cookie(&wrong).is_none());
    assert!(body(wrong).await.contains("Incorrect setup code"));
    let short = submit_setup(&router, &state, &code, "too short", "too short").await;
    assert_eq!(short.status(), StatusCode::BAD_REQUEST);
    assert!(body(short).await.contains("at least 12 characters"));
    let mismatch = submit_setup(&router, &state, &code, STORED, "stored admin passwore").await;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    let missing_csrf = post_form(
        &router,
        "/setup",
        &[
            ("setup_code", code.as_str()),
            ("password", STORED),
            ("confirm", STORED),
        ],
        &[],
    )
    .await;
    assert_eq!(missing_csrf.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let cross_origin = post_form(
        &router,
        "/setup",
        &[
            ("_csrf", state.csrf_token.as_str()),
            ("setup_code", code.as_str()),
            ("password", STORED),
            ("confirm", STORED),
        ],
        &[("origin", "https://attacker.example")],
    )
    .await;
    assert_eq!(cross_origin.status(), StatusCode::BAD_REQUEST);
    assert!(state.dashboard_setup_code().is_some());

    // Codes are accepted without dashes and in lower case.
    let done = submit_setup(
        &router,
        &state,
        &code.replace('-', "").to_lowercase(),
        STORED,
        STORED,
    )
    .await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&done), "/");
    let cookie = cookie_header(token_of(&session_cookie(&done).expect("setup logs in")));
    assert_eq!(
        get(&router, "/providers", &[("cookie", cookie.as_str())])
            .await
            .status(),
        StatusCode::OK
    );
    // Setup is over: the code is gone, setup redirects, and login is required.
    assert!(state.dashboard_setup_code().is_none());
    assert_eq!(location(&get(&router, "/setup", &[]).await), "/login");
    let again = submit_setup(
        &router,
        &state,
        &code,
        "another admin password",
        "another admin password",
    )
    .await;
    assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(location(&get(&router, "/providers", &[]).await), "/login");
    assert_eq!(
        login(&router, &state, "another admin password")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&router, &state, STORED).await.status(),
        StatusCode::SEE_OTHER
    );

    // Only an Argon2id hash is stored, never the password.
    let hash = db::dashboard_admin::password_hash(&state.db)
        .await
        .unwrap()
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains(STORED));
}

#[tokio::test]
async fn wrong_setup_codes_are_throttled_per_peer() {
    let state = stored_state("sqlite::memory:").await;
    let router = app::router(state.clone());
    for _ in 0..10 {
        let response = submit_setup(&router, &state, "AAAA-AAAA-AAAA-AAAA", STORED, STORED).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let code = state.dashboard_setup_code().unwrap();
    let blocked = submit_setup(&router, &state, &code, STORED, STORED).await;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(state.dashboard_setup_code().is_some());
}

#[tokio::test]
async fn the_stored_password_survives_restart_and_changing_it_revokes_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("auth.db").display());
    let first = stored_state(&url).await;
    let router = app::router(first.clone());
    let code = first.dashboard_setup_code().unwrap();
    assert_eq!(
        submit_setup(&router, &first, &code, STORED, STORED)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    first.db.close().await;

    // A restart keeps the password: no new setup code, and login is required.
    let state = stored_state(&url).await;
    let router = app::router(state.clone());
    assert!(state.dashboard_setup_code().is_none());
    assert_eq!(location(&get(&router, "/", &[]).await), "/login");
    let current = login(&router, &state, STORED).await;
    let current = cookie_header(token_of(&session_cookie(&current).unwrap()));
    let other = login(&router, &state, STORED).await;
    let other = cookie_header(token_of(&session_cookie(&other).unwrap()));
    let settings = get(&router, "/settings", &[("cookie", current.as_str())]).await;
    let html = body(settings).await;
    assert!(html.contains("action=\"/settings/password\""));
    assert!(html.contains("stored password (Argon2id hash)"));

    let change = |current_password: &'static str, password: &'static str, confirm: &'static str| {
        let router = router.clone();
        let state = state.clone();
        let cookie = current.clone();
        async move {
            post_form(
                &router,
                "/settings/password",
                &[
                    ("_csrf", state.csrf_token.as_str()),
                    ("current", current_password),
                    ("password", password),
                    ("confirm", confirm),
                ],
                &[("cookie", cookie.as_str())],
            )
            .await
        }
    };
    const NEW: &str = "replacement admin password";
    let wrong = change("not the password", NEW, NEW).await;
    assert_eq!(location(&wrong), "/settings?result=password-wrong");
    let mismatch = change(STORED, NEW, "replacement admin passwore").await;
    assert_eq!(location(&mismatch), "/settings?result=password-mismatch");
    let shown = get(
        &router,
        "/settings?result=password-mismatch",
        &[("cookie", current.as_str())],
    )
    .await;
    assert!(
        body(shown)
            .await
            .contains("The new passwords do not match.")
    );
    assert_eq!(
        location(&change(STORED, "short", "short").await),
        "/settings?result=password-short"
    );
    // Changing the password requires a session like every other dashboard POST.
    let anonymous = post_form(
        &router,
        "/settings/password",
        &[
            ("_csrf", state.csrf_token.as_str()),
            ("current", STORED),
            ("password", NEW),
            ("confirm", NEW),
        ],
        &[],
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let changed = change(STORED, NEW, NEW).await;
    assert_eq!(location(&changed), "/settings?result=password");
    let renewed = cookie_header(token_of(
        &session_cookie(&changed).expect("the changing browser stays signed in"),
    ));
    assert_eq!(
        get(&router, "/providers", &[("cookie", renewed.as_str())])
            .await
            .status(),
        StatusCode::OK
    );
    for revoked in [&current, &other] {
        assert_eq!(
            location(&get(&router, "/providers", &[("cookie", revoked.as_str())]).await),
            "/login"
        );
    }
    assert_eq!(
        login(&router, &state, STORED).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&router, &state, NEW).await.status(),
        StatusCode::SEE_OTHER
    );
    state.db.close().await;

    // Resetting removes the stored password and reopens setup on the next start.
    let pool = db::connect(&url).await.unwrap();
    assert!(db::dashboard_admin::reset(&pool).await.unwrap());
    pool.close().await;
    let reset = stored_state(&url).await;
    assert!(reset.dashboard_setup_code().is_some());
    assert_eq!(
        location(&get(&app::router(reset), "/", &[]).await),
        "/setup"
    );
}

#[tokio::test]
async fn an_environment_password_skips_setup_and_cannot_be_changed_in_the_dashboard() {
    let state = auth_state(Some(PASSWORD), false, None).await;
    let router = app::router(state.clone());
    assert!(state.dashboard_setup_code().is_none());
    assert_eq!(location(&get(&router, "/setup", &[]).await), "/login");
    let cookie = cookie_header(token_of(
        &session_cookie(&login(&router, &state, PASSWORD).await).unwrap(),
    ));
    let settings = body(get(&router, "/settings", &[("cookie", cookie.as_str())]).await).await;
    assert!(settings.contains("password from ROUTER_DASHBOARD_PASSWORD"));
    assert!(!settings.contains("action=\"/settings/password\""));
    let change = post_form(
        &router,
        "/settings/password",
        &[
            ("_csrf", state.csrf_token.as_str()),
            ("current", PASSWORD),
            ("password", "replacement admin password"),
            ("confirm", "replacement admin password"),
        ],
        &[("cookie", cookie.as_str())],
    )
    .await;
    assert_eq!(change.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        login(&router, &state, PASSWORD).await.status(),
        StatusCode::SEE_OTHER
    );
}
