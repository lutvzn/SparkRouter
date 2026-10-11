use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::SecretString;
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db,
    providers::Protocol,
};
use tower::ServiceExt;

async fn state(url: &str) -> AppState {
    AppState::from_config(Config {
        database_url: url.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([16; 32]))).unwrap(),
        api_key: None,
        dashboard_password: None,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}
async fn page(state: &AppState) -> String {
    let response = app::router(state.clone())
        .oneshot(Request::get("/providers").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("provider-initial"));
    assert!(html.contains("provider-title-link"));
    assert!(html.contains("provider-favorite-button"));
    html
}
fn favorites(html: &str) -> &str {
    html.split("id=\"provider-favorites-grid\"")
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap()
}
async fn save(
    state: &AppState,
    path: &str,
    fields: &str,
    csrf: &str,
    origin: Option<&str>,
) -> StatusCode {
    let mut request = Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("host", "localhost");
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    let response = app::router(state.clone())
        .oneshot(
            request
                .body(Body::from(format!("_csrf={csrf}&{fields}")))
                .unwrap(),
        )
        .await
        .unwrap();
    if response.status() == StatusCode::SEE_OTHER {
        assert_eq!(response.headers()["location"], "/providers");
    }
    response.status()
}

#[tokio::test]
async fn defaults_explicit_unfavorites_and_other_presets_persist() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("favorites.db").display());
    let state = state(&url).await;
    let html = page(&state).await;
    let top = favorites(&html);
    assert_eq!(top.matches("class=\"provider-card\"").count(), 3);
    for preset in ["anthropic", "openai", "openrouter"] {
        assert!(top.contains(&format!("href=\"/providers/catalog/{preset}\"")));
        assert_eq!(
            save(
                &state,
                &format!("/providers/catalog/{preset}/favorite"),
                "favorite=false",
                &state.csrf_token,
                None
            )
            .await,
            StatusCode::SEE_OTHER
        );
    }
    assert!(favorites(&page(&state).await).trim().ends_with("</div>"));
    assert!(!favorites(&page(&state).await).contains("provider-card"));
    assert!(page(&state).await.contains("No favorites yet"));
    let path = "/providers/catalog/groq/favorite";
    for _ in 0..2 {
        assert_eq!(
            save(&state, path, "favorite=true", &state.csrf_token, None).await,
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        favorites(&page(&state).await)
            .matches("provider-card")
            .count(),
        1
    );
    assert!(favorites(&page(&state).await).contains("/providers/catalog/groq"));
    state.db.close().await;
    let reopened = self::state(&url).await;
    let html = page(&reopened).await;
    assert_eq!(favorites(&html).matches("provider-card").count(), 1);
    assert!(favorites(&html).contains("/providers/catalog/groq"));
    // Unfavoriting does not remove cards from All providers or alter connections.
    let all = html.split("id=\"provider-grid\"").nth(1).unwrap();
    assert!(all.contains("href=\"/providers/catalog/openai\""));
    assert!(all.contains("aria-label=\"Favorite OpenAI\""));
    assert!(db::providers::list(&reopened.db).await.unwrap().is_empty());
    reopened.db.close().await;
}

#[tokio::test]
async fn custom_favorites_escape_names_and_cascade_without_endpoint_conflation() {
    let state = state("sqlite::memory:").await;
    let one = db::provider_groups::create_with_slug(
        &state.db,
        "<Favorite> & One",
        "https://example.com/v1",
        &Protocol::OpenAi,
        None,
    )
    .await
    .unwrap();
    let two = db::provider_groups::create_with_slug(
        &state.db,
        "Two",
        "https://example.com/v1",
        &Protocol::OpenAi,
        None,
    )
    .await
    .unwrap();
    let identity = format!("/providers/groups/{}", one.id);
    assert_eq!(
        save(
            &state,
            &format!("{identity}/favorite"),
            "favorite=true",
            &state.csrf_token,
            None
        )
        .await,
        StatusCode::SEE_OTHER
    );
    let html = page(&state).await;
    let top = favorites(&html);
    assert!(top.contains(&identity));
    assert!(!top.contains(&format!("/providers/groups/{}", two.id)));
    assert!(top.contains("Unfavorite &#60;Favorite&#62; &#38; One"));
    assert!(top.contains("aria-pressed=\"true\""));
    db::provider_groups::delete(&state.db, &one.id)
        .await
        .unwrap();
    assert!(
        !db::provider_favorites::list(&state.db)
            .await
            .unwrap()
            .contains_key(&identity)
    );
    assert!(!favorites(&page(&state).await).contains(&identity));
}

#[tokio::test]
async fn provider_tiles_sort_alphabetically_and_use_inline_bare_star_controls() {
    let state = state("sqlite::memory:").await;
    // Create in reverse name order; custom and preset tiles must share one ordering.
    for name in ["zz Custom", "middle Custom", "aa Custom"] {
        db::provider_groups::create_with_slug(
            &state.db,
            name,
            "https://example.com/v1",
            &Protocol::OpenAi,
            None,
        )
        .await
        .unwrap();
    }
    let html = page(&state).await;
    let all = html.split("id=\"provider-grid\"").nth(1).unwrap();
    let names: Vec<_> = all
        .split("class=\"provider-card\"")
        .skip(1)
        .map(|card| {
            card.split("class=\"provider-tile-title\">")
                .nth(1)
                .unwrap()
                .split("</span>")
                .next()
                .unwrap()
                .to_lowercase()
        })
        .collect();
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]), "{names:?}");
    assert!(
        names.iter().position(|name| name == "aa custom").unwrap()
            < names.iter().position(|name| name == "anthropic").unwrap()
    );
    assert!(
        names
            .iter()
            .position(|name| name == "middle custom")
            .unwrap()
            < names.iter().position(|name| name == "openai").unwrap()
    );
    assert!(names.contains(&"zz custom".into()));
    assert!(html.contains("viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\""));
    assert!(!html.contains('☆') && !html.contains('★'));
    let response = app::router(state.clone())
        .oneshot(Request::get("/static/app.css").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let css = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let button = css
        .split(".provider-favorite-button {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert!(button.contains("background: transparent"));
    assert!(button.contains("border: 0"));
    assert!(button.contains("border-radius: 0"));
    let title_row = html
        .split("class=\"provider-title-row\"")
        .nth(1)
        .unwrap()
        .split("</div>")
        .next()
        .unwrap();
    assert!(title_row.contains("class=\"provider-title-link\""));
    assert!(title_row.contains("class=\"provider-favorite-form\""));
    assert!(title_row.find("</a>").unwrap() < title_row.find("<form").unwrap());
    let form = css
        .split(".provider-favorite-form {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert!(form.contains("position: relative"));
    assert!(form.contains("margin-inline-start: auto"));
    assert!(!form.contains("position: absolute"));
    let row = css
        .split(".provider-card .provider-title-row {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert!(row.contains("flex: 1;"));
    assert!(css.contains(
        ".provider-favorite-button[aria-pressed=\"true\"] .icon { fill: currentColor; }"
    ));
}

#[tokio::test]
async fn favorite_writes_validate_identity_csrf_origin_and_boolean() {
    let state = state("sqlite::memory:").await;
    let path = "/providers/catalog/openai/favorite";
    let before = db::provider_favorites::list(&state.db).await.unwrap();
    assert_eq!(
        save(&state, path, "favorite=false", "wrong", None).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        save(
            &state,
            path,
            "favorite=false",
            &state.csrf_token,
            Some("https://other.example")
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    for path in [
        "/providers/catalog/missing/favorite",
        "/providers/groups/missing/favorite",
        "/providers/invalid/openai/favorite",
    ] {
        assert_eq!(
            save(&state, path, "favorite=true", &state.csrf_token, None).await,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        save(&state, path, "favorite=invalid", &state.csrf_token, None).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        db::provider_favorites::list(&state.db).await.unwrap(),
        before
    );
}
