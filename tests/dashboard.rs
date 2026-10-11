use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db,
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([15; 32]))).unwrap(),
        api_key: None,
        dashboard_password: None,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn submit(
    router: &Router,
    state: &AppState,
    path: &str,
    fields: &[(&str, &str)],
    htmx: bool,
) -> Response {
    submit_token(router, path, fields, htmx, &state.csrf_token).await
}

/// Like [`submit`] but with an explicit CSRF token, for rejection coverage.
async fn submit_token(
    router: &Router,
    path: &str,
    fields: &[(&str, &str)],
    htmx: bool,
    csrf: &str,
) -> Response {
    let mut url = reqwest::Url::parse("http://localhost/").unwrap();
    url.query_pairs_mut()
        .append_pair("_csrf", csrf)
        .extend_pairs(fields.iter().copied());
    router
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("hx-request", if htmx { "true" } else { "false" })
                .body(Body::from(url.query().unwrap().to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// GETs a dashboard page, asserting it renders successfully, and returns the HTML.
async fn get_page(router: &Router, path: &str) -> String {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {path} must render");
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

fn assert_dashboard_shell(html: &str, active_href: &str) {
    assert!(
        html.contains("<a class=\"skip-link\" href=\"#content\">"),
        "skip link targets content"
    );
    assert!(
        html.contains("<main id=\"content\" tabindex=\"-1\""),
        "main is the skip-link target and focusable"
    );
    let nav = html
        .find("<nav aria-label=\"Dashboard\">")
        .expect("dashboard nav");
    let nav_end = nav + html[nav..].find("</nav>").expect("nav closing");
    let nav = &html[nav..nav_end];
    let active = nav.matches("aria-current=\"page\"").count();
    assert_eq!(active, 1, "exactly one sidebar link is active");
    let link = nav.find("aria-current=\"page\"").unwrap();
    let start = nav[..link].rfind("<a ").unwrap();
    let tag_end = nav[start..].find('>').unwrap() + start;
    assert!(
        nav[start..tag_end].contains(&format!("href=\"{active_href}\"")),
        "active link must target {active_href}"
    );
    let topbar_start = html.find("<header class=\"topbar\">").expect("topbar");
    let topbar_end = topbar_start
        + html[topbar_start..]
            .find("</header>")
            .expect("topbar closing");
    let h1_count = html.matches("<h1>").count();
    assert_eq!(h1_count, 1, "each page has one h1");
    assert!(
        html[topbar_start..topbar_end].contains("<h1>"),
        "the only h1 belongs to the topbar"
    );
    let main_start = html.find("<main id=\"content\"").expect("main");
    let main_end = main_start + html[main_start..].find("</main>").expect("main closing");
    assert!(
        !html[main_start..main_end].contains("<h1>"),
        "main does not contain the page heading"
    );
}

/// Canonical custom-group id owning a stored connection, for index-href
/// assertions: tiles link the persisted group id, never the connection id.
async fn group_id_for(state: &AppState, connection_id: &str) -> String {
    db::provider_groups::by_connection(&state.db, connection_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("connection {connection_id} must belong to a custom group"))
        .id
}

/// Canonical group id extracted from a create redirect's Location header.
fn redirected_group_id(response: &Response) -> String {
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("custom provider creation redirects to the group page");
    location
        .strip_prefix("/providers/groups/")
        .expect("redirect targets a group page")
        .to_owned()
}

#[tokio::test]
async fn dashboard_crud_and_htmx_ordering_work_together() {
    let state = state().await;
    let router = app::router(state.clone());
    let secret = "private-dashboard-key";
    let created = submit(
        &router,
        &state,
        "/providers",
        &[
            ("name", "Primary <example>"),
            ("base_url", "https://example.invalid/v1/"),
            ("protocol", "openai"),
            ("api_key", secret),
            ("enabled", "on"),
        ],
        false,
    )
    .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let providers = db::providers::list(&state.db).await.unwrap();
    let provider = &providers[0];
    assert!(
        !provider
            .encrypted_api_key
            .windows(secret.len())
            .any(|part| part == secret.as_bytes())
    );
    assert_eq!(
        state
            .crypto
            .decrypt_api_key(&provider.id, &provider.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        secret
    );
    // The flat name lives on the edit form and the custom group page; the index
    // is a tile grid (name, protocol badges, count) with no flat rows anymore.
    // The legacy POST folds non-preset endpoints into a persisted custom group:
    // tiles link its canonical group id, while the pre-migration
    // /providers/groups/{connection-id} alias still resolves the same page.
    let group_id = db::provider_groups::by_connection(&state.db, &provider.id)
        .await
        .unwrap()
        .expect("legacy non-preset connection belongs to a custom group")
        .id;
    let group_path = format!("/providers/groups/{group_id}");
    let alias_path = format!("/providers/groups/{}", provider.id);
    for path in [
        "/providers".to_owned(),
        format!("/providers/{}", provider.id),
        group_path.clone(),
        alias_path.clone(),
    ] {
        let html = get_page(&router, &path).await;
        assert!(!html.contains(secret));
    }
    let edit = get_page(&router, &format!("/providers/{}", provider.id)).await;
    assert!(edit.contains("Primary &#60;example&#62;"));
    let group = get_page(&router, &group_path).await;
    assert!(group.contains("Primary &#60;example&#62;"));
    assert!(
        group.contains("https://example.invalid/v1"),
        "the group page keeps the endpoint URL the tile no longer shows"
    );
    let alias = get_page(&router, &alias_path).await;
    assert!(
        alias.contains("Primary &#60;example&#62;"),
        "the connection-id group alias must render the same escaped connection names"
    );
    let index = get_page(&router, "/providers").await;
    assert!(
        !index.contains("Primary &#60;example&#62;"),
        "index shows grouped tiles, not flat connection names"
    );
    assert!(index.contains("example.invalid"));
    assert!(!index.contains("<span class=\"badge\">OpenAI</span>"));
    assert!(
        !index.contains("https://example.invalid/v1"),
        "index tiles show protocol badges, not endpoint URLs"
    );
    assert!(index.contains(&format!("href=\"/providers/groups/{group_id}\"")));
    assert!(
        !index.contains(&format!("href=\"/providers/groups/{}\"", provider.id)),
        "the index href is the canonical group id, never the connection id"
    );
    let response = submit(&router, &state, "/combos", &[("name", "coding")], false).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let combo = db::combos::by_name(&state.db, "coding").await.unwrap();
    for model in ["first-model", "second-model"] {
        let response = submit(
            &router,
            &state,
            &format!("/combos/{}/steps", combo.id),
            &[("provider_id", &provider.id), ("model", model)],
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
    let steps = db::combos::steps(&state.db, &combo.id).await.unwrap();
    let response = submit(
        &router,
        &state,
        &format!("/combos/{}/steps/{}/up", combo.id, steps[1].id),
        &[],
        true,
    )
    .await;
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
    assert!(!html.contains("<html"));
    assert!(html.find("second-model").unwrap() < html.find("first-model").unwrap());
    let ordered = db::combos::steps(&state.db, &combo.id).await.unwrap();
    assert_eq!(ordered[0].model, "second-model");
    let response = submit(
        &router,
        &state,
        &format!("/providers/{}/delete", provider.id),
        &[],
        false,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "referenced provider must be protected"
    );
    let response = submit(
        &router,
        &state,
        &format!("/combos/{}", combo.id),
        &[("name", "renamed")],
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        db::combos::by_name(&state.db, "renamed").await.unwrap().id,
        combo.id
    );
    for step in ordered {
        let response = submit(
            &router,
            &state,
            &format!("/combos/{}/steps/{}/remove", combo.id, step.id),
            &[],
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/combos/{}/delete", combo.id),
            &[],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/providers/{}/delete", provider.id),
            &[],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(db::combos::list(&state.db).await.unwrap().is_empty());
    assert!(db::providers::list(&state.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn blank_key_provider_created_in_dashboard_routes_without_authorization() {
    let upstream = Router::new()
        .route("/models", get(|| async { Json(json!({"data":[]})) }))
        .route("/chat/completions", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert!(!headers.contains_key("authorization"));
            assert!(!headers.contains_key("x-api-key"));
            assert_eq!(body["model"], "local-model");
            Json(json!({"id":"local", "choices":[{"message":{"role":"assistant","content":"hello"}}]}))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Abort(server);
    let state = state().await;
    let router = app::router(state.clone());
    assert_eq!(
        submit(
            &router,
            &state,
            "/providers",
            &[
                ("name", "local"),
                ("base_url", &url),
                ("protocol", "openai"),
                ("api_key", ""),
                ("enabled", "on")
            ],
            false
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let provider = db::providers::list(&state.db).await.unwrap().remove(0);
    let response = submit(
        &router,
        &state,
        &format!("/providers/{}/test", provider.id),
        &[],
        true,
    )
    .await;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&bytes).contains("OK"));
    let combo = db::combos::create(&state.db, "local").await.unwrap();
    db::combos::add_step(&state.db, &combo.id, &provider.id, "local-model")
        .await
        .unwrap();
    let response = router
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"combo/local","messages":[{"role":"user","content":"hello"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn theme_toggle_script_loads_before_paint_and_csp_stays_strict() {
    let state = state().await;
    let router = app::router(state.clone());
    for path in [
        "/providers",
        "/combos",
        "/api-keys",
        "/providers/new",
        "/combos/new",
    ] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_owned();
        assert!(
            csp.contains("script-src 'self'"),
            "script-src must stay 'self'"
        );
        assert!(
            !csp.contains("unsafe-inline"),
            "CSP must not allow unsafe-inline"
        );
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
        let toggle_id = html
            .find("id=\"theme-toggle\"")
            .expect("theme toggle missing");
        let toggle_start = html[..toggle_id].rfind('<').expect("toggle element start");
        let toggle_end = toggle_id + html[toggle_id..].find('>').expect("toggle element end");
        let toggle = &html[toggle_start..=toggle_end];
        assert!(
            toggle.starts_with("<button "),
            "theme toggle must be a button"
        );
        assert!(
            toggle.contains("type=\"button\""),
            "toggle must not submit forms"
        );
        let theme_script = html.find("/static/theme.js").expect("theme.js reference");
        let stylesheet = html.find("/static/app.css").expect("stylesheet link");
        assert!(
            theme_script < stylesheet,
            "theme.js must be referenced before the stylesheet so the theme applies before paint"
        );
        let sidebar_open = html
            .find("<aside class=\"sidebar\">")
            .expect("sidebar opening");
        let sidebar_close = html[sidebar_open..]
            .find("</aside>")
            .expect("sidebar closing");
        let sidebar = &html[sidebar_open..sidebar_open + sidebar_close];
        assert!(
            sidebar.contains("<nav aria-label=\"Dashboard\">"),
            "sidebar must hold the dashboard navigation"
        );
        assert!(
            sidebar.contains("href=\"/providers\"")
                && sidebar.contains("href=\"/combos\"")
                && sidebar.contains("href=\"/api-keys\""),
            "sidebar nav must link to providers, combos, and API keys"
        );
        // Nav links carry decorative sprite icons: hidden from assistive
        // technology and referencing only the bundled local sprite.
        assert!(
            sidebar.contains("<svg")
                && sidebar.contains("aria-hidden=\"true\"")
                && sidebar.contains("focusable=\"false\""),
            "sidebar icons must be decorative SVG, hidden and unfocusable"
        );
        assert!(
            sidebar.contains("<use href=\"/static/icons.svg#server\"")
                && sidebar.contains("<use href=\"/static/icons.svg#layers\"")
                && sidebar.contains("<use href=\"/static/icons.svg#key\""),
            "sidebar links the local sprite's server, layers, and key symbols"
        );
        let topbar_open = html
            .find("<header class=\"topbar\">")
            .expect("topbar opening");
        let topbar_close = topbar_open
            + html[topbar_open..]
                .find("</header>")
                .expect("topbar closing");
        let topbar = &html[topbar_open..topbar_close];
        assert!(
            sidebar_open + sidebar_close <= topbar_open,
            "sidebar must come before the topbar"
        );
        assert!(
            topbar.contains("id=\"theme-toggle\""),
            "topbar must hold the theme toggle"
        );
        assert!(
            topbar.contains("btn-label"),
            "theme toggle keeps a visible .btn-label next to its icon"
        );
        assert!(
            topbar.contains("<use href=\"/static/icons.svg#sun\"")
                || topbar.contains("<use href=\"/static/icons.svg#moon\""),
            "theme toggle pairs its label with a local sun or moon sprite icon"
        );
        assert!(
            !html.contains("onclick"),
            "toggle behavior is wired in theme.js, never inline onclick"
        );
        assert!(
            !topbar.contains("<nav")
                && !topbar.contains("href=\"/providers\"")
                && !topbar.contains("href=\"/combos\"")
                && !topbar.contains("href=\"/api-keys\""),
            "topbar must not duplicate navigation links"
        );
        let active_href = match path {
            "/providers" | "/providers/new" => "/providers",
            "/combos" | "/combos/new" => "/combos",
            _ => "/api-keys",
        };
        assert_dashboard_shell(&html, active_href);
        // Index pages move their page heading and subtitle into the topbar.
        let main_open = html.find("<main").expect("main opening");
        let main_close = main_open + html[main_open..].find("</main>").expect("main closing");
        let main_section = &html[main_open..main_close];
        if path == "/api-keys" {
            assert!(topbar.contains("<h1>API keys</h1>"));
            assert!(topbar.contains(
                "Give each app its own gateway key, allowed models, and optional expiry."
            ));
            assert!(
                main_section.contains("Generate key")
                    && main_section.contains("name=\"name\"")
                    && main_section.contains("name=\"_csrf\""),
                "API keys page must include a generate form"
            );
            assert!(!main_section.contains("<h1>API keys</h1>"));
        } else if path == "/providers" || path == "/combos" {
            let (heading, subtitle, action_label, action_href) = if path == "/providers" {
                (
                    "<h1>Providers</h1>",
                    "Each tile is one upstream endpoint. Open a tile to manage its named connections; combos route to enabled connections.",
                    "Add custom provider",
                    "/providers/new",
                )
            } else {
                (
                    "<h1>Combos</h1>",
                    "Route requests through provider/model connections.",
                    "Add combo",
                    "/combos/new",
                )
            };
            assert!(
                topbar.contains(heading),
                "{path} topbar must carry the page heading"
            );
            assert!(
                topbar.contains("class=\"topbar-subtitle\""),
                "{path} topbar must carry the subtitle"
            );
            assert!(
                topbar.contains(subtitle),
                "{path} topbar subtitle text must stay intact"
            );
            assert!(
                !main_section.contains(heading),
                "{path} main must not repeat the topbar heading"
            );
            assert!(
                !main_section.contains("class=\"topbar-subtitle\""),
                "{path} main must not repeat the topbar subtitle"
            );
            assert!(
                main_section.contains(&format!("href=\"{action_href}\""))
                    && main_section.contains(action_label),
                "{path} main keeps its {action_label} action"
            );
        } else {
            let form_heading = if path == "/providers/new" {
                "<h1>Add custom provider</h1>"
            } else {
                "<h1>Add combo</h1>"
            };
            assert!(
                topbar.contains(form_heading),
                "{path} form heading belongs in topbar"
            );
            assert!(
                !main_section.contains(form_heading),
                "{path} main does not repeat its heading"
            );
        }
    }
    for (path, content_type) in [
        ("/static/theme.js", "text/javascript"),
        ("/static/app.css", "text/css"),
        ("/static/icons.svg", "image/svg+xml"),
        ("/static/icons-LICENSE.txt", "text/plain"),
    ] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path} must be served");
        let served = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_owned();
        assert!(
            served.starts_with(content_type),
            "{path} served as {served}, expected {content_type}"
        );
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        if path == "/static/theme.js" {
            assert!(
                !String::from_utf8_lossy(&bytes).trim().is_empty(),
                "theme.js must not be empty"
            );
        }
        if path == "/static/icons.svg" {
            let sprite = String::from_utf8_lossy(&bytes);
            for id in ["copy", "server", "sun", "moon", "key"] {
                assert!(
                    sprite.contains(&format!("id=\"{id}\"")),
                    "the sprite must define the {id} symbol"
                );
            }
            assert!(
                !sprite.contains("<script"),
                "the icon sprite must not embed scripts"
            );
            // Only the xmlns namespace declaration and the provenance comment
            // may carry a URL, so a blanket http ban would fail on both;
            // strip them and require every remaining reference to stay local.
            let references = sprite
                .replace("http://www.w3.org/2000/svg", "")
                .replace("https://lucide.dev", "");
            assert!(
                !references.contains("http"),
                "no external URL references beyond the namespace and provenance comment"
            );
        }
        if path == "/static/icons-LICENSE.txt" {
            let license = String::from_utf8_lossy(&bytes);
            for phrase in ["Lucide", "ISC License", "MIT License", "Cole Bemis"] {
                assert!(license.contains(phrase), "the license must credit {phrase}");
            }
        }
    }
}

#[tokio::test]
async fn providers_index_links_catalog_presets_and_blank_form_targets_them() {
    let state = state().await;
    let router = app::router(state.clone());
    let index = get_page(&router, "/providers").await;
    assert!(index.contains("class=\"provider-page\""));
    assert!(
        index.contains("id=\"provider-search\"")
            && index.contains("type=\"search\"")
            && index.contains("id=\"provider-grid\"")
            && index.contains("id=\"provider-search-empty\""),
        "providers index must include a live title filter"
    );
    assert!(
        !index
            .split("id=\"provider-search-empty\"")
            .next()
            .unwrap()
            .contains("<form"),
        "the providers index filter must not be a submitting form"
    );
    let script = get_page(&router, "/static/app.js").await;
    assert!(
        script.contains("getElementById(\"provider-search\")")
            && script.contains("addEventListener(\"input\"")
            && script.contains("provider-tile-title"),
        "provider search must filter titles on each input event"
    );
    assert!(index.contains("class=\"provider-tile-heading\""));
    assert!(index.contains("class=\"provider-tile-status\""));
    assert!(!index.contains("<span class=\"badge\">OpenAI</span>"));
    assert!(!index.contains("<span class=\"badge\">Anthropic</span>"));
    assert!(
        !index.contains("https://api.openai.com/v1"),
        "preset tiles show protocol badges, not the fixed endpoint"
    );
    for (slug, name) in [
        ("openai", "OpenAI"),
        ("anthropic", "Anthropic"),
        ("openrouter", "OpenRouter"),
        ("groq", "Groq"),
        ("deepseek", "DeepSeek"),
        ("together", "Together"),
        ("fireworks", "Fireworks"),
        ("xai", "xAI (Grok)"),
        ("poe", "Poe"),
        ("mistral", "Mistral"),
    ] {
        assert!(
            index.contains(&format!("href=\"/providers/catalog/{slug}\"")),
            "empty index must link the {name} preset tile"
        );
        assert!(
            index.contains(name),
            "preset tile shows the {name} display name"
        );
    }
    assert!(
        !index.contains("/providers/groups/"),
        "no custom tiles exist before any provider is stored"
    );
    let catalog = get_page(&router, "/providers/catalog/openai").await;
    assert!(
        catalog.contains("class=\"provider-page\""),
        "catalog preset pages share the provider-page group layout"
    );
    assert!(catalog.contains("OpenAI"));
    assert!(
        catalog.contains("https://api.openai.com/v1"),
        "catalog page shows the fixed endpoint"
    );
    assert!(catalog.contains("action=\"/providers/catalog/openai\""));
    for field in ["_csrf", "name", "api_key", "enabled"] {
        assert!(
            catalog.contains(&format!("name=\"{field}\"")),
            "add-connection form must include the {field} field"
        );
    }

    // Catalog preset pages share the group layout: the heading and endpoint
    // subtitle live in the topbar, metadata appears in a details card, and
    // the add-connection form opens in a native popover dialog.
    let topbar_open = catalog
        .find("<header class=\"topbar\">")
        .expect("catalog topbar opening");
    let topbar_close = topbar_open
        + catalog[topbar_open..]
            .find("</header>")
            .expect("catalog topbar closing");
    let topbar = &catalog[topbar_open..topbar_close];
    assert!(
        topbar.contains("<h1>OpenAI</h1>"),
        "the catalog topbar carries the preset name heading"
    );
    assert!(
        topbar.contains(
            "Every connection on this page uses this endpoint and stores its own API key."
        ),
        "the catalog topbar carries the endpoint explanatory subtitle"
    );
    assert!(
        !topbar.contains("<span class=\"badge\">Custom</span>"),
        "preset topbars carry no Custom badge"
    );
    assert!(
        catalog.contains("<section class=\"card provider-details\""),
        "endpoint metadata appears in a provider-details card"
    );
    assert!(
        catalog.contains("<dl class=\"provider-details-grid\""),
        "endpoint metadata renders in a provider-details grid"
    );
    assert!(
        catalog.contains("popovertarget=\"add-connection\""),
        "an opener control toggles the add-connection popover"
    );
    let popup_start = catalog
        .find("<div id=\"add-connection\" class=\"connection-popup\" popover role=\"dialog\" aria-labelledby=\"add-connection-title\">")
        .expect("add-connection popover dialog");
    let main_close = catalog.find("</main>").expect("catalog main closing");
    let popup = &catalog[popup_start..main_close];
    assert!(
        popup.contains("action=\"/providers/catalog/openai\""),
        "the add-connection form action lives inside the popup"
    );
    for field in ["_csrf", "name", "api_key", "enabled"] {
        assert!(
            popup.contains(&format!("name=\"{field}\"")),
            "the popup add-connection form includes the {field} field"
        );
    }
    assert!(
        popup.contains("popovertargetaction=\"hide\""),
        "the popup closes through the native popover hide action"
    );
}

#[tokio::test]
async fn catalog_connections_are_encrypted_pinned_and_labeled_in_combos() {
    let state = state().await;
    let router = app::router(state.clone());
    // Two named connections for the OpenAI preset. The submitted URL/protocol
    // are tampered with and must be overridden by the catalog values.
    let connections = [
        ("conn work", "sk-fake-work-key-111"),
        ("conn personal", "sk-fake-personal-key-222"),
    ];
    for (name, key) in connections {
        assert_eq!(
            submit(
                &router,
                &state,
                "/providers/catalog/openai",
                &[
                    ("name", name),
                    ("base_url", "https://attacker.invalid/v1"),
                    ("protocol", "anthropic"),
                    ("api_key", key),
                    ("enabled", "on"),
                ],
                false
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let stored = db::providers::list(&state.db).await.unwrap();
    assert_eq!(stored.len(), 2, "both connections are stored provider rows");
    let work = stored
        .iter()
        .find(|p| p.name == "conn work")
        .expect("work connection stored");
    let personal = stored
        .iter()
        .find(|p| p.name == "conn personal")
        .expect("personal connection stored");
    for provider in [work, personal] {
        assert_eq!(provider.base_url, "https://api.openai.com/v1");
        assert_eq!(provider.protocol.as_str(), "openai");
        assert!(provider.enabled);
    }
    assert_ne!(
        work.encrypted_api_key, personal.encrypted_api_key,
        "random nonces must keep the two envelopes distinct"
    );
    for (provider, key) in [
        (work, "sk-fake-work-key-111"),
        (personal, "sk-fake-personal-key-222"),
    ] {
        assert!(
            !provider
                .encrypted_api_key
                .windows(key.len())
                .any(|part| part == key.as_bytes())
        );
        assert_eq!(
            state
                .crypto
                .decrypt_api_key(&provider.id, &provider.encrypted_api_key)
                .unwrap()
                .expose_secret(),
            key,
            "ciphertext must decrypt under its own provider id (AAD)"
        );
    }
    // The AAD binds each envelope to its row: cross-record decryption fails.
    assert!(
        state
            .crypto
            .decrypt_api_key(&personal.id, &work.encrypted_api_key)
            .is_err()
    );
    assert!(
        state
            .crypto
            .decrypt_api_key(&work.id, &personal.encrypted_api_key)
            .is_err()
    );

    let catalog = get_page(&router, "/providers/catalog/openai").await;
    assert!(catalog.contains("conn work") && catalog.contains("conn personal"));
    assert!(
        !catalog.contains("sk-fake-work-key-111") && !catalog.contains("sk-fake-personal-key-222"),
        "raw keys must never reach the page"
    );
    let index = get_page(&router, "/providers").await;
    assert!(index.contains("href=\"/providers/catalog/openai\""));
    assert!(
        !index.contains("/providers/groups/"),
        "catalog endpoints are never custom tiles"
    );
    for (name, _) in connections {
        assert!(
            !index.contains(name),
            "index shows counts, not connection names"
        );
    }
    assert!(
        index.contains("2 connection") || index.contains("2 enabled"),
        "the OpenAI tile must show its connection count"
    );

    // Combo pickers label each connection row and select by row id.
    assert_eq!(
        submit(&router, &state, "/combos", &[("name", "picker")], false)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let combo = db::combos::by_name(&state.db, "picker").await.unwrap();
    let edit = get_page(&router, &format!("/combos/{}", combo.id)).await;
    assert_dashboard_shell(&edit, "/combos");
    assert!(edit.contains("<option value=\"preset:openai\">OpenAI</option>"));
    assert!(edit.contains(&format!(
        "<option value=\"{}\" data-provider=\"preset:openai\">conn work</option>",
        work.id
    )));
    assert!(edit.contains(&format!(
        "<option value=\"{}\" data-provider=\"preset:openai\">conn personal</option>",
        personal.id
    )));
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/combos/{}/steps", combo.id),
            &[("provider_id", &work.id), ("model", "fake-model-openai")],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let steps = db::combos::steps(&state.db, &combo.id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].provider_id, work.id.as_str());
    assert_eq!(steps[0].model, "fake-model-openai");
}

#[tokio::test]
async fn custom_tiles_group_by_exact_endpoint_and_reject_unknown_or_bad_csrf() {
    let state = state().await;
    let router = app::router(state.clone());
    // Existing rows created through the unchanged flat POST /providers flow.
    let seed = [
        (
            "shared one",
            "https://llm.internal.example/v1",
            "openai",
            true,
        ),
        (
            "shared two",
            "https://llm.internal.example/v1",
            "openai",
            false,
        ),
        (
            "protocol flip",
            "https://llm.internal.example/v1",
            "anthropic",
            true,
        ),
        (
            "prefix shift",
            "https://llm.internal.example/openai/v1",
            "openai",
            true,
        ),
    ];
    for (name, base_url, protocol, enabled) in seed {
        let mut fields = vec![
            ("name", name),
            ("base_url", base_url),
            ("protocol", protocol),
            ("api_key", ""),
        ];
        if enabled {
            fields.push(("enabled", "on"));
        }
        assert_eq!(
            submit(&router, &state, "/providers", &fields, false)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
    }
    let stored = db::providers::list(&state.db).await.unwrap();
    assert_eq!(stored.len(), 4);
    let id_of = |name: &str| {
        stored
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{name} provider missing"))
            .id
            .clone()
    };
    let shared_one = id_of("shared one");
    let shared_two = id_of("shared two");
    let protocol_flip = id_of("protocol flip");
    let prefix_shift = id_of("prefix shift");
    // The legacy POST auto-assigns every non-preset endpoint to a persisted
    // custom group keyed by exact base_url+protocol.
    let shared_group = group_id_for(&state, &shared_one).await;
    assert_eq!(
        group_id_for(&state, &shared_two).await,
        shared_group,
        "rows with an identical base_url+protocol share one persisted group"
    );
    let protocol_flip_group = group_id_for(&state, &protocol_flip).await;
    let prefix_shift_group = group_id_for(&state, &prefix_shift).await;
    assert_ne!(
        protocol_flip_group, shared_group,
        "the same endpoint under the other protocol is a separate group"
    );
    assert_ne!(
        prefix_shift_group, shared_group,
        "the same host with a different path prefix is a separate group"
    );
    let tile = |id: &str| format!("href=\"/providers/groups/{id}\"");

    let index = get_page(&router, "/providers").await;
    assert!(
        index.contains(&tile(&shared_group)),
        "the shared endpoint tile links the canonical group id"
    );
    assert!(
        index.contains(&tile(&protocol_flip_group)),
        "the same endpoint under the other protocol is a separate tile"
    );
    assert!(
        index.contains(&tile(&prefix_shift_group)),
        "the same host with a different path prefix is a separate tile"
    );
    for (name, _, _, _) in seed {
        assert!(!index.contains(name), "index shows counts, not flat names");
    }
    assert!(index.contains("llm.internal.example"));
    assert!(!index.contains("<span class=\"badge\">Anthropic</span>"));
    assert!(
        !index.contains("https://llm.internal.example/v1"),
        "index tiles show protocol badges, not endpoint URLs"
    );
    assert!(!index.contains("https://llm.internal.example/openai/v1"));
    assert!(
        !index.contains("/delete") && !index.contains("hx-post=\"/providers/"),
        "index tiles carry no per-row actions"
    );
    assert!(
        index.contains("1 of 2") || index.contains("1 enabled"),
        "the shared tile must surface its enabled-connection count"
    );

    let group_path = format!("/providers/groups/{shared_group}");
    let group = get_page(&router, &group_path).await;
    assert_dashboard_shell(&group, "/providers");
    assert!(group.contains("shared one") && group.contains("shared two"));
    assert!(!group.contains("protocol flip") && !group.contains("prefix shift"));
    assert!(group.contains("https://llm.internal.example/v1"));
    assert!(group.contains(&format!("action=\"{group_path}\"")));
    for field in ["_csrf", "name", "api_key", "enabled"] {
        assert!(
            group.contains(&format!("name=\"{field}\"")),
            "group add-connection form must include the {field} field"
        );
    }

    // The pre-migration /providers/groups/{connection-id} alias still resolves
    // this group, so stale tile links keep working.
    let alias = get_page(&router, &format!("/providers/groups/{shared_one}")).await;
    assert!(alias.contains("shared one") && alias.contains("shared two"));
    assert!(!alias.contains("protocol flip") && !alias.contains("prefix shift"));

    // Adding through the group form keeps the group endpoint despite tampering.
    assert_eq!(
        submit(
            &router,
            &state,
            &group_path,
            &[
                ("name", "joined later"),
                ("base_url", "https://evil.example/v1"),
                ("protocol", "anthropic"),
                ("api_key", "sk-fake-group-key-333"),
                ("enabled", "on"),
            ],
            false
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let after_join = db::providers::list(&state.db).await.unwrap();
    let joined = after_join
        .iter()
        .find(|p| p.name == "joined later")
        .expect("group submission stored");
    assert_eq!(joined.base_url, "https://llm.internal.example/v1");
    assert_eq!(joined.protocol.as_str(), "openai");
    assert_eq!(
        state
            .crypto
            .decrypt_api_key(&joined.id, &joined.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        "sk-fake-group-key-333"
    );
    let joined_id = joined.id.clone();
    assert_eq!(
        group_id_for(&state, &joined_id).await,
        shared_group,
        "adding a connection through the group form keeps the group id"
    );
    let group = get_page(&router, &group_path).await;
    assert!(group.contains("joined later"));
    assert!(!group.contains("sk-fake-group-key-333"));
    let index = get_page(&router, "/providers").await;
    assert!(index.contains(&tile(&shared_group)));
    assert!(!index.contains(&tile(&joined_id)));

    // Unknown presets and groups are 404s for GET and POST, never empty pages.
    for path in [
        "/providers/catalog/unknown-preset",
        "/providers/groups/00000000-0000-0000-0000-000000000000",
    ] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {path}");
        assert_eq!(
            submit(
                &router,
                &state,
                path,
                &[
                    ("name", "ghost connection"),
                    ("base_url", "https://api.openai.com/v1"),
                    ("protocol", "openai"),
                    ("api_key", ""),
                    ("enabled", "on"),
                ],
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "POST {path}"
        );
    }

    // Invalid CSRF is a 400 and leaves storage untouched.
    let rows_before = db::providers::list(&state.db).await.unwrap().len();
    for path in [
        "/providers".to_owned(),
        "/providers/catalog/openai".to_owned(),
        group_path.clone(),
    ] {
        assert_eq!(
            submit_token(
                &router,
                &path,
                &[
                    ("name", "csrf bypass"),
                    ("base_url", "https://api.openai.com/v1"),
                    ("protocol", "openai"),
                    ("api_key", "sk-fake"),
                    ("enabled", "on"),
                ],
                false,
                "definitely-not-the-token",
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST,
            "POST {path} must reject an invalid CSRF token"
        );
    }
    let final_rows = db::providers::list(&state.db).await.unwrap();
    assert_eq!(
        final_rows.len(),
        rows_before,
        "rejected posts must not write rows"
    );
    assert!(final_rows.iter().all(|p| p.name != "csrf bypass"));
}

#[tokio::test]
async fn custom_provider_post_creates_empty_named_group_card() {
    let state = state().await;
    let router = app::router(state.clone());

    // The custom form collects name/base_url/protocol only: credentials and
    // the enabled flag belong to connections, added later on the group page.
    let form = get_page(&router, "/providers/new").await;
    assert!(form.contains("action=\"/providers/custom\""));
    for field in ["_csrf", "name", "slug", "base_url", "protocol"] {
        assert!(
            form.contains(&format!("name=\"{field}\"")),
            "custom provider form must include the {field} field"
        );
    }
    assert!(
        !form.contains("name=\"api_key\"") && !form.contains("name=\"enabled\""),
        "the custom form must not collect credentials or the enabled flag"
    );

    let created = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Acme <LLM> & Co"),
            ("base_url", "https://acme.invalid/v1/"),
            ("protocol", "openai"),
        ],
        false,
    )
    .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let group_id = redirected_group_id(&created);
    uuid::Uuid::parse_str(
        group_id
            .strip_prefix("custom-")
            .expect("group ids use the custom-<uuid> shape"),
    )
    .expect("group ids use the custom-<uuid> shape");

    // One empty persisted group with normalized URL and escaped name; no
    // Provider rows exist until a connection is added.
    let groups = db::provider_groups::list(&state.db).await.unwrap();
    assert_eq!(
        groups.len(),
        1,
        "creating a custom provider stores one group"
    );
    assert_eq!(groups[0].group.id, group_id);
    assert_eq!(groups[0].group.name, "Acme <LLM> & Co");
    assert_eq!(groups[0].group.base_url, "https://acme.invalid/v1");
    assert_eq!(groups[0].group.protocol.as_str(), "openai");
    assert_eq!(groups[0].connection_count, 0);
    assert!(
        db::providers::list(&state.db).await.unwrap().is_empty(),
        "no Provider rows exist before any connection is added"
    );

    // The empty card is visible on the index before any key exists.
    let index = get_page(&router, "/providers").await;
    assert!(index.contains(&format!("href=\"/providers/groups/{group_id}\"")));
    assert!(index.contains("Acme &#60;LLM&#62; &#38; Co"));
    assert!(index.contains("not connected"));
    assert!(!index.contains("<span class=\"badge\">OpenAI</span>"));
    assert!(index.contains("<span class=\"badge\">Custom</span>"));
    assert!(
        !index.contains("https://acme.invalid/v1"),
        "the index tile shows badges, not the endpoint URL"
    );

    let group = get_page(&router, &format!("/providers/groups/{group_id}")).await;
    assert!(group.contains("Acme &#60;LLM&#62; &#38; Co"));
    assert!(group.contains("https://acme.invalid/v1"));
    assert!(group.contains("<code>openai</code>"));
    assert!(group.contains("No connections for this endpoint yet"));

    // Group-page layout: the heading and endpoint subtitle live in the
    // topbar, the Custom badge renders in the body after the header, the
    // endpoint metadata folds into a details card, and the add-connection
    // form opens in a native popover dialog.
    let topbar_open = group
        .find("<header class=\"topbar\">")
        .expect("group topbar opening");
    let topbar_close = topbar_open
        + group[topbar_open..]
            .find("</header>")
            .expect("group topbar closing");
    let topbar = &group[topbar_open..topbar_close];
    assert!(
        topbar.contains("<h1>Acme &#60;LLM&#62; &#38; Co</h1>"),
        "the group topbar carries the escaped group name heading"
    );
    assert!(
        topbar.contains(
            "Every connection on this page uses this endpoint and stores its own API key."
        ),
        "the group topbar carries the endpoint explanatory subtitle"
    );
    assert!(
        !topbar.contains("<span class=\"badge\">Custom</span>"),
        "the Custom badge moved out of the topbar"
    );
    assert!(
        group[topbar_close..].contains("<span class=\"badge\">Custom</span>"),
        "the group Custom badge renders in the body after the header"
    );
    assert!(
        group.contains("class=\"provider-page\""),
        "the group page uses the provider-page main layout"
    );
    assert!(
        group.contains("<section class=\"card provider-details\""),
        "endpoint metadata appears in a provider-details card"
    );
    assert!(
        group.contains("<dl class=\"provider-details-grid\""),
        "endpoint metadata renders in a provider-details grid"
    );
    assert!(
        group.contains("popovertarget=\"add-connection\""),
        "an opener control toggles the add-connection popover"
    );
    let popup_start = group
        .find("<div id=\"add-connection\" class=\"connection-popup\" popover role=\"dialog\" aria-labelledby=\"add-connection-title\">")
        .expect("add-connection popover dialog");
    let main_close = group.find("</main>").expect("group main closing");
    let popup = &group[popup_start..main_close];
    assert!(
        popup.contains(&format!("action=\"/providers/groups/{group_id}\"")),
        "the add-connection form action lives inside the popup"
    );
    for field in ["_csrf", "name", "api_key", "enabled"] {
        assert!(
            popup.contains(&format!("name=\"{field}\"")),
            "the popup add-connection form includes the {field} field"
        );
    }
    assert!(
        popup.contains("popovertargetaction=\"hide\""),
        "the popup closes through the native popover hide action"
    );
}

#[tokio::test]
async fn custom_group_keys_counts_metadata_and_validation_rejections() {
    let state = state().await;
    let router = app::router(state.clone());

    // Invalid CSRF, a blank name, a bad URL, and an unknown protocol are all
    // 400s that write no rows.
    assert_eq!(
        submit_token(
            &router,
            "/providers/custom",
            &[
                ("name", "csrf bypass"),
                ("base_url", "https://beta.invalid/v1"),
                ("protocol", "anthropic"),
            ],
            false,
            "definitely-not-the-token",
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "POST /providers/custom must reject an invalid CSRF token"
    );
    for fields in [
        [
            ("name", "   "),
            ("base_url", "https://beta.invalid/v1"),
            ("protocol", "anthropic"),
        ],
        [
            ("name", "Beta <LLM>"),
            ("base_url", "not a valid url"),
            ("protocol", "anthropic"),
        ],
        [
            ("name", "Beta <LLM>"),
            ("base_url", "https://beta.invalid/v1"),
            ("protocol", "carrier-pigeon"),
        ],
    ] {
        assert_eq!(
            submit(&router, &state, "/providers/custom", &fields, false)
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "rejected custom provider form: {fields:?}"
        );
    }
    assert!(
        db::provider_groups::list(&state.db)
            .await
            .unwrap()
            .is_empty(),
        "rejected posts must not create groups"
    );
    assert!(db::providers::list(&state.db).await.unwrap().is_empty());

    let created = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Beta <LLM>"),
            ("base_url", "https://beta.invalid/v1"),
            ("protocol", "anthropic"),
        ],
        false,
    )
    .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let group_id = redirected_group_id(&created);
    let group_path = format!("/providers/groups/{group_id}");

    // Two named keys: one encrypted credential, one blank no-auth connection.
    // The submitted endpoint is tampered with and must stay pinned to the
    // group's stored base URL and protocol.
    for (name, key, enabled) in [
        ("work key", "sk-beta-work-key-1", true),
        ("local noauth", "", false),
    ] {
        let mut fields = vec![
            ("name", name),
            ("base_url", "https://evil.example/v1"),
            ("protocol", "openai"),
            ("api_key", key),
        ];
        if enabled {
            fields.push(("enabled", "on"));
        }
        assert_eq!(
            submit(&router, &state, &group_path, &fields, false)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
    }
    let stored = db::providers::list(&state.db).await.unwrap();
    assert_eq!(stored.len(), 2);
    for provider in &stored {
        assert_eq!(provider.base_url, "https://beta.invalid/v1");
        assert_eq!(provider.protocol.as_str(), "anthropic");
        assert_eq!(
            group_id_for(&state, &provider.id).await,
            group_id,
            "every added connection joins the creating group"
        );
    }
    let work = stored
        .iter()
        .find(|p| p.name == "work key")
        .expect("work key connection stored");
    assert!(
        !work
            .encrypted_api_key
            .windows("sk-beta-work-key-1".len())
            .any(|part| part == b"sk-beta-work-key-1")
    );
    assert_eq!(
        state
            .crypto
            .decrypt_api_key(&work.id, &work.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        "sk-beta-work-key-1"
    );

    // Pages show connection names and persisted group metadata, never raw keys.
    let group = get_page(&router, &group_path).await;
    assert!(group.contains("work key") && group.contains("local noauth"));
    assert!(!group.contains("sk-beta-work-key-1"));
    assert!(group.contains("Beta &#60;LLM&#62;"));
    assert!(group.contains("https://beta.invalid/v1"));
    assert!(group.contains("<code>anthropic</code>"));
    let index = get_page(&router, "/providers").await;
    assert!(index.contains(&format!("href=\"/providers/groups/{group_id}\"")));
    assert!(index.contains("Beta &#60;LLM&#62;"));
    assert!(index.contains("2 connection"));
    assert!(index.contains("1 enabled"));
    assert!(!index.contains("<span class=\"badge\">Anthropic</span>"));
    assert!(index.contains("<span class=\"badge\">Custom</span>"));
    assert!(
        !index.contains("https://beta.invalid/v1"),
        "index tiles show protocol badges, not endpoint URLs"
    );

    // Deleting keys removes connections; the group card itself survives,
    // even after the last connection is gone.
    let work_id = work.id.clone();
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/providers/{work_id}/delete"),
            &[],
            false
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(db::providers::list(&state.db).await.unwrap().len(), 1);
    let remaining = db::providers::list(&state.db).await.unwrap().remove(0);
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/providers/{}/delete", remaining.id),
            &[],
            false
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(db::providers::list(&state.db).await.unwrap().is_empty());
    let groups = db::provider_groups::list(&state.db).await.unwrap();
    assert_eq!(groups.len(), 1, "the emptied group keeps its card");
    assert_eq!(groups[0].group.id, group_id);
    assert_eq!(groups[0].connection_count, 0);
    let index = get_page(&router, "/providers").await;
    assert!(index.contains(&format!("href=\"/providers/groups/{group_id}\"")));
    assert!(index.contains("not connected"));
}

#[tokio::test]
async fn custom_provider_slug_is_persisted_and_must_be_unique_and_non_reserved() {
    let state = state().await;
    let router = app::router(state.clone());
    let fields = [
        ("name", "Provider One"),
        ("slug", "provider1"),
        ("base_url", "https://slug.test/v1"),
        ("protocol", "openai"),
    ];
    let created = submit(&router, &state, "/providers/custom", &fields, false).await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let group_id = redirected_group_id(&created);
    assert_eq!(
        db::provider_slugs::for_group(&state.db, &group_id)
            .await
            .unwrap(),
        "provider1"
    );
    let page = get_page(&router, &format!("/providers/groups/{group_id}")).await;
    assert!(
        page.contains("provider1"),
        "group details display its routing slug"
    );
    assert!(page.contains("Direct model ID: <code>provider1/&lt;upstream-model-id&gt;</code>"));
    assert!(page.contains("Combo steps use upstream IDs without this prefix."));

    let fields = [
        ("name", "Duplicate"),
        ("slug", "provider1"),
        ("base_url", "https://slug.test/v1"),
        ("protocol", "openai"),
    ];
    let duplicate = submit(&router, &state, "/providers/custom", &fields, false).await;
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
    assert_eq!(db::provider_groups::list(&state.db).await.unwrap().len(), 1);

    for slug in ["combo", "openrouter", "ds", "bad/path"] {
        let fields = [
            ("name", "Rejected"),
            ("slug", slug),
            ("base_url", "https://slug.test/v1"),
            ("protocol", "openai"),
        ];
        let response = submit(&router, &state, "/providers/custom", &fields, false).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "slug {slug}");
        assert_eq!(db::provider_groups::list(&state.db).await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn omitted_custom_provider_slug_is_generated_stably_and_resolves_collisions() {
    let state = state().await;
    let router = app::router(state.clone());
    let fields = [
        ("name", "Acme LLM"),
        ("base_url", "https://generated.test/v1"),
        ("protocol", "openai"),
    ];
    let first = submit(&router, &state, "/providers/custom", &fields, false).await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);
    let first_id = redirected_group_id(&first);
    let second = submit(&router, &state, "/providers/custom", &fields, false).await;
    assert_eq!(second.status(), StatusCode::SEE_OTHER);
    let second_id = redirected_group_id(&second);
    let first_slug = db::provider_slugs::for_group(&state.db, &first_id)
        .await
        .unwrap();
    let second_slug = db::provider_slugs::for_group(&state.db, &second_id)
        .await
        .unwrap();
    assert_ne!(
        first_slug, second_slug,
        "colliding generated slugs get a suffix"
    );
    let group_page_path = format!("/providers/groups/{first_id}");
    let initial = get_page(&router, &group_page_path).await;
    let repeated = get_page(&router, &group_page_path).await;
    assert_eq!(initial, repeated, "GET does not mutate/regenerate the slug");
    assert!(initial.contains(&first_slug));
}

#[tokio::test]
async fn named_custom_group_at_preset_endpoint_is_separate_and_labels_combos() {
    let state = state().await;
    let router = app::router(state.clone());

    // Two plain connections on the OpenAI preset page.
    for name in ["conn work", "conn personal"] {
        assert_eq!(
            submit(
                &router,
                &state,
                "/providers/catalog/openai",
                &[
                    ("name", name),
                    ("base_url", "https://api.openai.com/v1"),
                    ("protocol", "openai"),
                    ("api_key", "sk-fake-preset-key"),
                    ("enabled", "on"),
                ],
                false,
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }

    // A named custom provider reusing the exact OpenAI default endpoint.
    let work_created = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Work OpenAI"),
            ("base_url", "https://api.openai.com/v1"),
            ("protocol", "openai"),
        ],
        false,
    )
    .await;
    assert_eq!(work_created.status(), StatusCode::SEE_OTHER);
    let work_group = redirected_group_id(&work_created);
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/providers/groups/{work_group}"),
            &[
                ("name", "team key"),
                ("base_url", "https://evil.example/v1"),
                ("protocol", "anthropic"),
                ("api_key", "sk-fake-team-key-444"),
                ("enabled", "on"),
            ],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );

    // A second named custom provider on the very same endpoint stays separate.
    let personal_created = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Personal OpenAI"),
            ("base_url", "https://api.openai.com/v1"),
            ("protocol", "openai"),
        ],
        false,
    )
    .await;
    assert_eq!(personal_created.status(), StatusCode::SEE_OTHER);
    let personal_group = redirected_group_id(&personal_created);
    assert_ne!(
        work_group, personal_group,
        "same-endpoint named custom providers are never merged"
    );
    let groups = db::provider_groups::list(&state.db).await.unwrap();
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .any(|g| g.group.id == work_group && g.group.name == "Work OpenAI")
    );
    assert!(
        groups
            .iter()
            .any(|g| g.group.id == personal_group && g.group.name == "Personal OpenAI")
    );

    // The grouped connection is stored on the group's pinned endpoint and is a
    // member, so the preset tile and page never count or list it.
    let stored = db::providers::list(&state.db).await.unwrap();
    let team = stored
        .iter()
        .find(|p| p.name == "team key")
        .expect("team connection stored");
    assert_eq!(team.base_url, "https://api.openai.com/v1");
    assert_eq!(team.protocol.as_str(), "openai");
    assert_eq!(group_id_for(&state, &team.id).await, work_group);
    let index = get_page(&router, "/providers").await;
    assert!(index.contains("href=\"/providers/catalog/openai\""));
    assert!(index.contains(&format!("href=\"/providers/groups/{work_group}\"")));
    assert!(index.contains(&format!("href=\"/providers/groups/{personal_group}\"")));
    assert!(index.contains("Work OpenAI") && index.contains("Personal OpenAI"));
    assert!(
        index.contains("2 connection"),
        "the preset tile counts only its unassigned connections"
    );
    let catalog = get_page(&router, "/providers/catalog/openai").await;
    assert!(catalog.contains("conn work") && catalog.contains("conn personal"));
    assert!(
        !catalog.contains("team key"),
        "grouped connections never appear on the preset page"
    );

    // Combo pickers label grouped connections with the stored group name;
    // preset connections keep their preset label.
    assert_eq!(
        submit(&router, &state, "/combos", &[("name", "labels")], false)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let combo = db::combos::by_name(&state.db, "labels").await.unwrap();
    let preset_row = stored
        .iter()
        .find(|p| p.name == "conn work")
        .expect("preset connection stored");
    let edit = get_page(&router, &format!("/combos/{}", combo.id)).await;
    assert!(edit.contains("<option value=\"preset:openai\">OpenAI</option>"));
    assert!(edit.contains(&format!(
        "<option value=\"group:{work_group}\">Work OpenAI</option>"
    )));
    assert!(edit.contains(&format!(
        "<option value=\"{}\" data-provider=\"preset:openai\">conn work</option>",
        preset_row.id
    )));
    assert!(edit.contains(&format!(
        "<option value=\"{}\" data-provider=\"group:{work_group}\">team key</option>",
        team.id
    )));
}

#[tokio::test]
async fn custom_group_delete_guards_csrf_origin_aliases_and_unknown_ids() {
    let state = state().await;
    let router = app::router(state.clone());

    // A custom group with two keys: one encrypted credential and one blank
    // no-auth connection, both added through the group page form.
    let created = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Gamma <LLM>"),
            ("base_url", "https://gamma.invalid/v1"),
            ("protocol", "openai"),
        ],
        false,
    )
    .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let group_id = redirected_group_id(&created);
    let group_path = format!("/providers/groups/{group_id}");
    let delete_path = format!("/providers/groups/{group_id}/delete");
    for (name, key) in [("work key", "sk-gamma-work-key-9"), ("local noauth", "")] {
        assert_eq!(
            submit(
                &router,
                &state,
                &group_path,
                &[
                    ("name", name),
                    ("base_url", "https://evil.example/v1"),
                    ("protocol", "anthropic"),
                    ("api_key", key),
                    ("enabled", "on"),
                ],
                false,
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let stored = db::providers::list(&state.db).await.unwrap();
    assert_eq!(stored.len(), 2);
    let work_id = stored
        .iter()
        .find(|p| p.name == "work key")
        .expect("work key stored")
        .id
        .clone();

    // The custom group page carries the badge and the canonical delete form
    // (with its own CSRF input); the index tile is badged Custom exactly once.
    let group_html = get_page(&router, &group_path).await;
    assert!(group_html.contains("<span class=\"badge\">Custom</span>"));
    let form_start = group_html
        .find(&format!("action=\"{delete_path}\""))
        .expect("custom group delete form");
    assert!(
        group_html
            .get(form_start..form_start + 200)
            .is_some_and(|snippet| snippet.contains("name=\"_csrf\"")),
        "the group delete form carries a CSRF input"
    );
    let index = get_page(&router, "/providers").await;
    assert_eq!(
        index.matches("<span class=\"badge\">Custom</span>").count(),
        1,
        "only the custom tile carries the Custom badge"
    );

    // Preset pages keep per-connection controls only: no Custom badge and no
    // group-level delete form.
    assert_eq!(
        submit(
            &router,
            &state,
            "/providers/catalog/openai",
            &[
                ("name", "preset key"),
                ("base_url", "https://api.openai.com/v1"),
                ("protocol", "openai"),
                ("api_key", "sk-preset-key-7"),
                ("enabled", "on"),
            ],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let catalog = get_page(&router, "/providers/catalog/openai").await;
    assert!(catalog.contains("preset key"));
    assert!(!catalog.contains("Delete provider"));
    assert!(!catalog.contains("/providers/catalog/openai/delete"));
    assert!(!catalog.contains("<span class=\"badge\">Custom</span>"));

    // An invalid CSRF token is a 400 that deletes nothing.
    assert_eq!(
        submit_token(
            &router,
            &delete_path,
            &[],
            false,
            "definitely-not-the-token",
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(db::provider_groups::get(&state.db, &group_id).await.is_ok());

    // Cross-origin submissions are rejected before storage even with a valid
    // CSRF token: a foreign Origin or a cross-site fetch annotation.
    let mut url = reqwest::Url::parse("http://localhost/").unwrap();
    url.query_pairs_mut()
        .append_pair("_csrf", state.csrf_token.as_str());
    let csrf_body = url.query().unwrap().to_owned();
    for (origin, fetch_site) in [
        (Some("https://attacker.example"), None),
        (None, Some("cross-site")),
    ] {
        let mut request = Request::post(delete_path.as_str())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("host", "localhost");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if let Some(fetch_site) = fetch_site {
            request = request.header("sec-fetch-site", fetch_site);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(csrf_body.clone())).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "cross-origin group delete must be rejected"
        );
    }
    assert!(db::provider_groups::get(&state.db, &group_id).await.is_ok());
    assert_eq!(db::providers::list(&state.db).await.unwrap().len(), 3);

    // Connection-id aliases, unknown group ids, and preset slugs are 404s:
    // there is no bulk delete through legacy links.
    for path in [
        format!("/providers/groups/{work_id}/delete"),
        "/providers/groups/00000000-0000-0000-0000-000000000000/delete".to_owned(),
        "/providers/groups/openai/delete".to_owned(),
    ] {
        assert_eq!(
            submit(&router, &state, &path, &[], false).await.status(),
            StatusCode::NOT_FOUND,
            "POST {path} must not delete anything"
        );
    }
    assert!(db::provider_groups::get(&state.db, &group_id).await.is_ok());
    assert_eq!(db::providers::list(&state.db).await.unwrap().len(), 3);
}

#[tokio::test]
async fn custom_group_delete_blocked_by_combo_then_removes_group_and_members() {
    let state = state().await;
    let router = app::router(state.clone());

    // The doomed group holds two keys; a second custom group and unassigned
    // preset keys must survive its deletion.
    let created = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Gamma <LLM>"),
            ("base_url", "https://gamma.invalid/v1"),
            ("protocol", "openai"),
        ],
        false,
    )
    .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let group_id = redirected_group_id(&created);
    let group_path = format!("/providers/groups/{group_id}");
    let delete_path = format!("/providers/groups/{group_id}/delete");
    for (name, key) in [("work key", "sk-gamma-work-key-9"), ("local noauth", "")] {
        assert_eq!(
            submit(
                &router,
                &state,
                &group_path,
                &[
                    ("name", name),
                    ("base_url", "https://evil.example/v1"),
                    ("protocol", "anthropic"),
                    ("api_key", key),
                    ("enabled", "on"),
                ],
                false,
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let kept = submit(
        &router,
        &state,
        "/providers/custom",
        &[
            ("name", "Delta host"),
            ("base_url", "https://delta.invalid/v1"),
            ("protocol", "anthropic"),
        ],
        false,
    )
    .await;
    assert_eq!(kept.status(), StatusCode::SEE_OTHER);
    let kept_id = redirected_group_id(&kept);
    let kept_path = format!("/providers/groups/{kept_id}");
    assert_eq!(
        submit(
            &router,
            &state,
            &kept_path,
            &[
                ("name", "delta key"),
                ("base_url", "https://evil.example/v1"),
                ("protocol", "openai"),
                ("api_key", "sk-delta-key-2"),
                ("enabled", "on"),
            ],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        submit(
            &router,
            &state,
            "/providers/catalog/openai",
            &[
                ("name", "preset key"),
                ("base_url", "https://api.openai.com/v1"),
                ("protocol", "openai"),
                ("api_key", "sk-preset-key-7"),
                ("enabled", "on"),
            ],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );

    let stored = db::providers::list(&state.db).await.unwrap();
    assert_eq!(stored.len(), 4);
    let work_id = stored
        .iter()
        .find(|p| p.name == "work key")
        .expect("work key stored")
        .id
        .clone();

    // A combo step referencing one key blocks the whole group deletion.
    assert_eq!(
        submit(&router, &state, "/combos", &[("name", "guard")], false)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let combo = db::combos::by_name(&state.db, "guard").await.unwrap();
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/combos/{}/steps", combo.id),
            &[("provider_id", &work_id), ("model", "gamma-model")],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        submit(&router, &state, &delete_path, &[], false)
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "a combo-referenced group cannot be deleted"
    );
    // The rejected deletion leaves the group, all four keys, and the step.
    assert!(db::provider_groups::get(&state.db, &group_id).await.is_ok());
    assert_eq!(db::providers::list(&state.db).await.unwrap().len(), 4);
    let work_row = db::providers::get(&state.db, &work_id).await.unwrap();
    assert_eq!(
        state
            .crypto
            .decrypt_api_key(&work_id, &work_row.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        "sk-gamma-work-key-9",
        "the blocked delete must not touch stored ciphertext"
    );
    assert_eq!(
        db::combos::steps(&state.db, &combo.id).await.unwrap().len(),
        1
    );

    // Removing the combo releases the deletion: it removes the group card and
    // every member credential in one transaction, then redirects to the index.
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/combos/{}/delete", combo.id),
            &[],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let deleted = submit(&router, &state, &delete_path, &[], false).await;
    assert_eq!(deleted.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        deleted
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/providers")
    );

    let groups = db::provider_groups::list(&state.db).await.unwrap();
    assert_eq!(groups.len(), 1, "only the untouched group remains");
    assert_eq!(groups[0].group.id, kept_id);
    assert_eq!(groups[0].connection_count, 1);
    let survivors = db::providers::list(&state.db).await.unwrap();
    let names: Vec<&str> = survivors.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["delta key", "preset key"]);
    assert!(
        db::provider_groups::by_connection(&state.db, &work_id)
            .await
            .unwrap()
            .is_none(),
        "deleted members keep no membership rows"
    );
    let response = router
        .clone()
        .oneshot(
            Request::get(group_path.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let index = get_page(&router, "/providers").await;
    assert!(!index.contains(&format!("href=\"/providers/groups/{group_id}\"")));
    assert!(index.contains(&format!("href=\"/providers/groups/{kept_id}\"")));
    assert!(index.contains("href=\"/providers/catalog/openai\""));
}

#[tokio::test]
async fn combo_duplicate_clones_steps_with_fresh_ids_and_guards_csrf_and_origin() {
    let state = state().await;
    let router = app::router(state.clone());

    // One upstream connection through the dashboard form; the source combo and
    // its two ordered steps are seeded through the db helpers.
    assert_eq!(
        submit(
            &router,
            &state,
            "/providers",
            &[
                ("name", "Dup upstream"),
                ("base_url", "https://dup.invalid/v1"),
                ("protocol", "openai"),
                ("api_key", "sk-dup-seed-key-5"),
                ("enabled", "on"),
            ],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let provider = db::providers::list(&state.db).await.unwrap().remove(0);
    let source = db::combos::create(&state.db, "source").await.unwrap();
    for model in ["first-model", "second-model"] {
        db::combos::add_step(&state.db, &source.id, &provider.id, model)
            .await
            .unwrap();
    }
    let duplicate_path = format!("/combos/{}/duplicate", source.id);
    fn redirected_combo_id(response: &Response) -> String {
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("duplicate redirects to the new combo's editor")
            .strip_prefix("/combos/")
            .expect("redirect targets a combo editor")
            .to_owned()
    }

    // An invalid CSRF token is a 400 that duplicates nothing.
    assert_eq!(
        submit_token(
            &router,
            &duplicate_path,
            &[],
            false,
            "definitely-not-the-token",
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "POST {duplicate_path} must reject an invalid CSRF token"
    );
    assert_eq!(db::combos::list(&state.db).await.unwrap().len(), 1);

    // Cross-origin submissions are rejected before storage even with a valid
    // CSRF token: a foreign Origin or a cross-site fetch annotation.
    let mut url = reqwest::Url::parse("http://localhost/").unwrap();
    url.query_pairs_mut()
        .append_pair("_csrf", state.csrf_token.as_str());
    let csrf_body = url.query().unwrap().to_owned();
    for (origin, fetch_site) in [
        (Some("https://attacker.example"), None),
        (None, Some("cross-site")),
    ] {
        let mut request = Request::post(duplicate_path.as_str())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("host", "localhost");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if let Some(fetch_site) = fetch_site {
            request = request.header("sec-fetch-site", fetch_site);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(csrf_body.clone())).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "cross-origin combo duplicate must be rejected"
        );
    }
    assert_eq!(
        db::combos::list(&state.db).await.unwrap().len(),
        1,
        "rejected posts must not create combos"
    );

    // First duplicate redirects to the new combo's editor (303) and clones the
    // steps with fresh ids in the same provider/model order.
    let first = submit(&router, &state, &duplicate_path, &[], false).await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);
    let first_id = redirected_combo_id(&first);
    assert_ne!(first_id, source.id);
    assert_eq!(
        db::combos::get(&state.db, &first_id).await.unwrap().name,
        "source-copy"
    );
    let source_steps = db::combos::steps(&state.db, &source.id).await.unwrap();
    let cloned_steps = db::combos::steps(&state.db, &first_id).await.unwrap();
    assert_eq!(
        cloned_steps.len(),
        source_steps.len(),
        "every source step is cloned"
    );
    for (index, (source_step, cloned_step)) in source_steps.iter().zip(&cloned_steps).enumerate() {
        assert_ne!(cloned_step.id, source_step.id, "cloned steps get fresh ids");
        assert_eq!(cloned_step.combo_id, first_id);
        assert_eq!(cloned_step.position, index as i64);
        assert_eq!(cloned_step.provider_id, provider.id);
        assert_eq!(cloned_step.model, source_step.model);
    }

    // A second duplicate of the same source lands on source-copy-2.
    let second = submit(&router, &state, &duplicate_path, &[], false).await;
    assert_eq!(second.status(), StatusCode::SEE_OTHER);
    let second_id = redirected_combo_id(&second);
    assert_ne!(second_id, source.id);
    assert_ne!(second_id, first_id);
    assert_eq!(
        db::combos::get(&state.db, &second_id).await.unwrap().name,
        "source-copy-2"
    );

    // The source combo is untouched: same name, same steps, same step ids.
    assert_eq!(
        db::combos::get(&state.db, &source.id).await.unwrap().name,
        "source"
    );
    let after = db::combos::steps(&state.db, &source.id).await.unwrap();
    let models: Vec<&str> = after.iter().map(|step| step.model.as_str()).collect();
    assert_eq!(models, ["first-model", "second-model"]);
    let source_ids: Vec<&str> = source_steps.iter().map(|step| step.id.as_str()).collect();
    assert!(
        after
            .iter()
            .all(|step| source_ids.contains(&step.id.as_str()))
    );
    assert_eq!(
        db::combos::list(&state.db).await.unwrap().len(),
        3,
        "source plus two duplicates"
    );

    // Unknown sources are 404s, never empty duplicates.
    assert_eq!(
        submit(
            &router,
            &state,
            "/combos/00000000-0000-0000-0000-000000000000/duplicate",
            &[],
            false,
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    // The index row carries the copy-model affordance, the title link, and the
    // duplicate/edit/delete actions.
    let index = get_page(&router, "/combos").await;
    assert!(index.contains(&format!("action=\"/combos/{}/duplicate\"", source.id)));
    assert!(index.contains(&format!("href=\"/combos/{}\"", source.id)));
    assert!(index.contains(&format!("action=\"/combos/{}/delete\"", source.id)));
    assert!(index.contains("data-copy-model=\"combo/source\""));
    assert!(index.contains(">source</a>"));

    // Editing or deleting copies must not mutate the original chain.
    db::combos::remove_step(&state.db, &first_id, &cloned_steps[0].id)
        .await
        .unwrap();
    db::combos::delete(&state.db, &second_id).await.unwrap();
    assert_eq!(
        db::combos::steps(&state.db, &first_id).await.unwrap().len(),
        1
    );
    let unchanged = db::combos::steps(&state.db, &source.id).await.unwrap();
    assert_eq!(unchanged.len(), 2);
    for (before, after) in source_steps.iter().zip(unchanged) {
        assert_eq!(before.id, after.id);
        assert_eq!(before.position, after.position);
        assert_eq!(before.model, after.model);
    }
}

#[tokio::test]
async fn combo_strategy_defaults_edits_and_validation() {
    let state = state().await;
    let router = app::router(state.clone());

    // The create form offers the three strategies with priority preselected.
    let form = get_page(&router, "/combos/new").await;
    assert!(form.contains("<select id=\"strategy\" name=\"strategy\""));
    assert!(form.contains("<option value=\"priority\" selected>Priority</option>"));
    assert!(form.contains("<option value=\"round_robin\">Round robin</option>"));
    assert!(form.contains("<option value=\"random\">Random</option>"));

    // Creating without (or with a blank) strategy defaults to priority.
    assert_eq!(
        submit(&router, &state, "/combos", &[("name", "plain")], false)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        db::combos::by_name(&state.db, "plain")
            .await
            .unwrap()
            .strategy
            .as_str(),
        "priority",
        "legacy name-only creation defaults to priority"
    );
    assert_eq!(
        submit(
            &router,
            &state,
            "/combos",
            &[("name", "blank"), ("strategy", "")],
            false
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        db::combos::by_name(&state.db, "blank")
            .await
            .unwrap()
            .strategy
            .as_str(),
        "priority"
    );

    // Each explicit strategy persists through creation.
    for (name, strategy) in [("robin", "round_robin"), ("dice", "random")] {
        assert_eq!(
            submit(
                &router,
                &state,
                "/combos",
                &[("name", name), ("strategy", strategy)],
                false
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            db::combos::by_name(&state.db, name)
                .await
                .unwrap()
                .strategy
                .as_str(),
            strategy
        );
    }

    // An unknown strategy is a 400 that writes no row.
    assert_eq!(
        submit(
            &router,
            &state,
            "/combos",
            &[("name", "bogus"), ("strategy", "shuffle")],
            false
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "unknown create strategy must be rejected"
    );
    assert!(
        db::combos::by_name(&state.db, "bogus").await.is_err(),
        "the rejected create must not write a combo row"
    );

    // The index rows keep their step summary and show a strategy badge each.
    let index = get_page(&router, "/combos").await;
    assert!(index.contains(">no steps</span>"));
    assert!(index.contains("<span class=\"badge\">Priority</span>"));
    assert!(index.contains("<span class=\"badge\">Round robin</span>"));
    assert!(index.contains("<span class=\"badge\">Random</span>"));

    // Editing persists the selected strategy and marks it selected on reload.
    let plain = db::combos::by_name(&state.db, "plain").await.unwrap();
    let edit_path = format!("/combos/{}", plain.id);
    assert_eq!(
        submit(
            &router,
            &state,
            &edit_path,
            &[("name", "plain"), ("strategy", "random")],
            false,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        db::combos::by_name(&state.db, "plain")
            .await
            .unwrap()
            .strategy
            .as_str(),
        "random"
    );
    let edit = get_page(&router, &edit_path).await;
    assert!(edit.contains("<option value=\"random\" selected>Random</option>"));
    assert!(
        !edit.contains("value=\"priority\" selected")
            && !edit.contains("value=\"round_robin\" selected"),
        "only the stored strategy may be selected"
    );
    assert!(edit.contains("Save changes"));

    // An invalid edit strategy is a 400 that applies no partial rename.
    assert_eq!(
        submit(
            &router,
            &state,
            &edit_path,
            &[("name", "plain-renamed"), ("strategy", "shuffle")],
            false,
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "unknown edit strategy must be rejected before any write"
    );
    let stored = db::combos::by_name(&state.db, "plain").await.unwrap();
    assert_eq!(stored.strategy.as_str(), "random");
    assert!(
        db::combos::by_name(&state.db, "plain-renamed")
            .await
            .is_err(),
        "the rejected edit must not rename the combo either"
    );

    // A legacy name-only rename still works and preserves the strategy.
    assert_eq!(
        submit(&router, &state, &edit_path, &[("name", "plain2")], false)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        db::combos::by_name(&state.db, "plain2")
            .await
            .unwrap()
            .strategy
            .as_str(),
        "random",
        "name-only edits preserve the stored strategy"
    );

    // Duplicating a combo copies its strategy along with its steps.
    let robin = db::combos::by_name(&state.db, "robin").await.unwrap();
    let duplicated = submit(
        &router,
        &state,
        &format!("/combos/{}/duplicate", robin.id),
        &[],
        false,
    )
    .await;
    assert_eq!(duplicated.status(), StatusCode::SEE_OTHER);
    let copy_id = duplicated
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .and_then(|location| location.strip_prefix("/combos/"))
        .expect("duplicate redirects to the new combo's editor")
        .to_owned();
    assert_eq!(
        db::combos::get(&state.db, &copy_id)
            .await
            .unwrap()
            .strategy
            .as_str(),
        "round_robin",
        "duplicates keep the source strategy"
    );
}

#[tokio::test]
async fn generated_api_key_is_shown_once_and_revoked_without_leaking() {
    let state = state().await;
    let router = app::router(state.clone());
    assert_eq!(
        submit(&router, &state, "/api-keys", &[("name", " ")], false)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(db::api_keys::list(&state.db).await.unwrap().is_empty());

    let created = submit(&router, &state, "/api-keys", &[("name", " laptop ")], false).await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let location = created
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("create redirects to the one-time reveal page")
        .to_owned();
    let reveal = get_page(&router, &location).await;
    let secret = reveal
        .split("class=\"key-secret\">")
        .nth(1)
        .and_then(|rest| rest.split("</code>").next())
        .expect("reveal page shows the secret once");
    assert!(
        secret.starts_with("sk-"),
        "generated keys use the sk- prefix"
    );
    assert!(reveal.contains("laptop"));
    let again = get_page(&router, &location).await;
    assert!(
        !again.contains(secret),
        "a second visit must not show the secret"
    );
    assert!(again.contains("cannot be displayed again"));

    let index = get_page(&router, "/api-keys").await;
    assert!(index.contains("laptop"));
    assert!(
        !index.contains(secret),
        "the key list must not contain the secret"
    );
    let stored: String = sqlx::query_scalar("SELECT key_hash FROM api_keys")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(!stored.contains(secret));

    let id = location.trim_start_matches("/api-keys/");
    assert_eq!(
        submit(
            &router,
            &state,
            &format!("/api-keys/{id}/delete"),
            &[],
            false
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(db::api_keys::list(&state.db).await.unwrap().is_empty());
}
