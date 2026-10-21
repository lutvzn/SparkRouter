//! End-to-end load test: how many simultaneous users can one gateway serve?
//!
//! Everything crosses real sockets. The harness starts a mock OpenAI-compatible
//! upstream (this executable re-run as `stress mock`) and the release
//! `sparkrouter` binary with a disposable database, configures them through the
//! dashboard's HTTP forms exactly as a person would, then ramps virtual users
//! that send chat traffic to `/v1` with a generated gateway key. Gateway memory,
//! CPU, threads and open files are sampled from `/proc` (Linux).
//!
//! ```sh
//! cargo build --release --locked
//! cargo run --release --example stress -- --levels 10,50,100,200
//! cargo run --release --example stress -- --help
//! ```
//!
//! No real provider is ever contacted unless `--url` points at a gateway that
//! is itself configured with real providers.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path as UrlPath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde_json::{Value, json};

const MOCK_MODEL: &str = "stress-model";

const HELP: &str = "\
SparkRouter stress test (real processes, real HTTP)

USAGE:
  cargo run --release --example stress -- [OPTIONS]
  cargo run --release --example stress -- mock [MOCK OPTIONS]   # standalone mock upstream

LOAD
  --levels LIST            Simultaneous users per step          [1,10,25,50,100,200,400]
  --duration SECS          Measured seconds per step             [20]
  --warmup SECS            Unmeasured ramp-in per step           [3]
  --think-ms MS            Pause between a user's requests       [0]
  --timeout-secs SECS      Per-request client timeout            [60]
  --stream-ratio R         Share of chat requests that stream    [0.5]
  --direct-ratio R         Share using provider/model, not combo [0.2]
  --models-ratio R         Share of GET /v1/models requests      [0.02]

TARGETS (a step that misses either ends the ramp)
  --slo-p99-ms MS          Highest acceptable p99 latency        [2000]
  --slo-success PCT        Lowest acceptable success rate        [99.5]
  --keep-going             Run every level even after a miss

MOCK UPSTREAM
  --upstream-ttfb-ms MS    Delay before the first byte           [200]
  --upstream-jitter-ms MS  +/- random variation of that delay    [50]
  --chunks N               Output chunks (tokens) per response   [20]
  --chunk-ms MS            Delay between output chunks           [15]
  --upstream-error-rate R  Primary returns 503 (combo falls back) [0]

GATEWAY
  --gateway-bin PATH       Binary to start  [target/release/sparkrouter]
  --logging MODE           Request history: off | basic | extended (default: gateway default)
  --history-buffer N       History write buffer multiple: 1 | 2 | 4 | 8 (default: gateway default)
  --out DIR                Run directory for logs, DB and report.json [target/stress/<time>]

EXTERNAL GATEWAY (skips spawning and setup; may incur provider charges)
  --url URL --api-key KEY --model ID [--pid PID for /proc metrics]
";

// ---------------------------------------------------------------- options

#[derive(Clone)]
struct MockConfig {
    ttfb_ms: u64,
    jitter_ms: u64,
    chunks: u32,
    chunk_ms: u64,
    error_rate: f64,
}

struct External {
    url: String,
    api_key: String,
    model: String,
    pid: Option<u32>,
}

struct Options {
    levels: Vec<usize>,
    duration: Duration,
    warmup: Duration,
    think: Duration,
    timeout: Duration,
    stream_ratio: f64,
    direct_ratio: f64,
    models_ratio: f64,
    slo_p99_ms: f64,
    slo_success: f64,
    keep_going: bool,
    mock: MockConfig,
    gateway_bin: PathBuf,
    logging: Option<String>,
    history_buffer: Option<String>,
    out: Option<PathBuf>,
    external: Option<External>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            levels: vec![1, 10, 25, 50, 100, 200, 400],
            duration: Duration::from_secs(20),
            warmup: Duration::from_secs(3),
            think: Duration::ZERO,
            timeout: Duration::from_secs(60),
            stream_ratio: 0.5,
            direct_ratio: 0.2,
            models_ratio: 0.02,
            slo_p99_ms: 2000.0,
            slo_success: 99.5,
            keep_going: false,
            mock: MockConfig {
                ttfb_ms: 200,
                jitter_ms: 50,
                chunks: 20,
                chunk_ms: 15,
                error_rate: 0.0,
            },
            gateway_bin: Path::new(env!("CARGO_MANIFEST_DIR")).join("target/release/sparkrouter"),
            logging: None,
            history_buffer: None,
            out: None,
            external: None,
        }
    }
}

fn parse<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid value for {name}: {value}"))
}

fn ratio(name: &str, value: &str) -> Result<f64, String> {
    let value: f64 = parse(name, value)?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between 0 and 1"))
    }
}

/// Parses `--name value` pairs shared by the load test and the mock.
fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let (mut url, mut api_key, mut model, mut pid) = (None, None, None, None);
    let mut args = args.iter();
    while let Some(name) = args.next() {
        if name == "--keep-going" {
            options.keep_going = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{name} needs a value (see --help)"))?;
        let secs = |v: &str| parse::<u64>(name, v).map(Duration::from_secs);
        match name.as_str() {
            "--levels" => {
                options.levels = value
                    .split(',')
                    .map(|level| parse::<usize>(name, level.trim()))
                    .collect::<Result<_, _>>()?;
                if options.levels.is_empty() || options.levels.contains(&0) {
                    return Err("--levels needs positive user counts".into());
                }
            }
            "--duration" => options.duration = secs(value)?,
            "--warmup" => options.warmup = secs(value)?,
            "--think-ms" => options.think = Duration::from_millis(parse(name, value)?),
            "--timeout-secs" => options.timeout = secs(value)?,
            "--stream-ratio" => options.stream_ratio = ratio(name, value)?,
            "--direct-ratio" => options.direct_ratio = ratio(name, value)?,
            "--models-ratio" => options.models_ratio = ratio(name, value)?,
            "--slo-p99-ms" => options.slo_p99_ms = parse(name, value)?,
            "--slo-success" => options.slo_success = parse(name, value)?,
            "--upstream-ttfb-ms" => options.mock.ttfb_ms = parse(name, value)?,
            "--upstream-jitter-ms" => options.mock.jitter_ms = parse(name, value)?,
            "--chunks" => options.mock.chunks = parse(name, value)?,
            "--chunk-ms" => options.mock.chunk_ms = parse(name, value)?,
            "--upstream-error-rate" => options.mock.error_rate = ratio(name, value)?,
            "--gateway-bin" => options.gateway_bin = PathBuf::from(value),
            "--logging" => match value.as_str() {
                "off" | "basic" | "extended" => options.logging = Some(value.clone()),
                _ => return Err("--logging must be off, basic or extended".into()),
            },
            "--history-buffer" => match value.as_str() {
                "1" | "2" | "4" | "8" => options.history_buffer = Some(value.clone()),
                _ => return Err("--history-buffer must be 1, 2, 4 or 8".into()),
            },
            "--out" => options.out = Some(PathBuf::from(value)),
            "--url" => url = Some(value.trim_end_matches('/').to_owned()),
            "--api-key" => api_key = Some(value.clone()),
            "--model" => model = Some(value.clone()),
            "--pid" => pid = Some(parse(name, value)?),
            _ => return Err(format!("unknown option {name} (see --help)")),
        }
    }
    if options.duration.is_zero() || options.mock.chunks == 0 {
        return Err("--duration and --chunks must be positive".into());
    }
    match (url, api_key, model) {
        (None, None, None) if pid.is_none() => {}
        (Some(url), Some(api_key), Some(model)) => {
            options.external = Some(External {
                url,
                api_key,
                model,
                pid,
            });
        }
        _ => return Err("--url, --api-key and --model must be given together".into()),
    }
    Ok(options)
}

// ---------------------------------------------------------------- randomness

/// xorshift64*: dependency-free and good enough for traffic mixing.
struct Rng(u64);

impl Rng {
    fn seeded() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let seed = nanos ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

// ---------------------------------------------------------------- mock upstream

fn chunk_event(index: u32, created: u64) -> String {
    let chunk = json!({
        "id": "chatcmpl-stress",
        "object": "chat.completion.chunk",
        "created": created,
        "model": MOCK_MODEL,
        "choices": [{"index": 0, "delta": {"content": format!("token{index} ")}, "finish_reason": null}],
    });
    format!("data: {chunk}\n\n")
}

async fn mock_models() -> Json<Value> {
    Json(
        json!({"object": "list", "data": [{"id": MOCK_MODEL, "object": "model", "owned_by": "stress"}]}),
    )
}

async fn mock_chat(
    State(config): State<Arc<MockConfig>>,
    UrlPath(tier): UrlPath<String>,
    body: Bytes,
) -> Response {
    let mut rng = Rng::seeded();
    if tier == "primary" && rng.unit() < config.error_rate {
        let error = json!({"error": {"message": "injected overload", "type": "server_error"}});
        return (StatusCode::SERVICE_UNAVAILABLE, Json(error)).into_response();
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream = request["stream"].as_bool().unwrap_or(false);
    let include_usage = request["stream_options"]["include_usage"]
        .as_bool()
        .unwrap_or(false);
    let prompt_tokens = body.len() as u64 / 4;
    let jitter = rng.below(config.jitter_ms * 2 + 1) as i64 - config.jitter_ms as i64;
    let ttfb = Duration::from_millis(config.ttfb_ms.saturating_add_signed(jitter));
    let chunk_delay = Duration::from_millis(config.chunk_ms);
    let chunks = config.chunks;
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let usage = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": chunks,
        "total_tokens": prompt_tokens + u64::from(chunks),
    });
    tokio::time::sleep(ttfb).await;
    if !stream {
        // A non-streaming upstream still spends the generation time.
        tokio::time::sleep(chunk_delay * chunks.saturating_sub(1)).await;
        let content: String = (0..chunks).map(|i| format!("token{i} ")).collect();
        return Json(json!({
            "id": "chatcmpl-stress",
            "object": "chat.completion",
            "created": created,
            "model": MOCK_MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": usage,
        }))
        .into_response();
    }
    let events = async_stream::stream! {
        for index in 0..chunks {
            if index > 0 {
                tokio::time::sleep(chunk_delay).await;
            }
            yield Ok::<_, Infallible>(Bytes::from(chunk_event(index, created)));
        }
        if include_usage {
            let last = json!({"id": "chatcmpl-stress", "object": "chat.completion.chunk", "created": created,
                "model": MOCK_MODEL, "choices": [], "usage": usage});
            yield Ok(Bytes::from(format!("data: {last}\n\n")));
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(events))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Serves `/{primary,backup}/v1/...` and prints `MOCK_PORT=<port>` once bound.
async fn run_mock(config: MockConfig, port: u16) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("mock bind failed: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    println!("MOCK_PORT={port}");
    let app = Router::new()
        .route("/{tier}/v1/models", get(mock_models))
        .route("/{tier}/v1/chat/completions", post(mock_chat))
        .with_state(Arc::new(config));
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("mock server failed: {e}"))
}

// ---------------------------------------------------------------- processes

/// Kills the child when dropped, including on Ctrl-C or an early error.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(|e| format!("no free port: {e}"))
}

fn log_file(dir: &Path, name: &str) -> Result<std::fs::File, String> {
    std::fs::File::create(dir.join(name)).map_err(|e| format!("cannot create {name}: {e}"))
}

fn spawn_mock(config: &MockConfig, dir: &Path) -> Result<(ChildGuard, u16), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg("mock")
        .args(["--port", "0"])
        .args(["--upstream-ttfb-ms", &config.ttfb_ms.to_string()])
        .args(["--upstream-jitter-ms", &config.jitter_ms.to_string()])
        .args(["--chunks", &config.chunks.to_string()])
        .args(["--chunk-ms", &config.chunk_ms.to_string()])
        .args(["--upstream-error-rate", &config.error_rate.to_string()])
        .stdout(Stdio::piped())
        .stderr(log_file(dir, "mock.log")?)
        .spawn()
        .map_err(|e| format!("cannot start mock upstream: {e}"))?;
    let stdout = child.stdout.take().ok_or("mock has no stdout")?;
    let guard = ChildGuard(child);
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let port = line
        .trim()
        .strip_prefix("MOCK_PORT=")
        .and_then(|port| port.parse().ok())
        .ok_or_else(|| format!("mock did not report a port (see {})", dir.display()))?;
    Ok((guard, port))
}

fn spawn_gateway(options: &Options, dir: &Path, port: u16) -> Result<ChildGuard, String> {
    if !options.gateway_bin.exists() {
        return Err(format!(
            "{} not found; run `cargo build --release --locked` first or pass --gateway-bin",
            options.gateway_bin.display()
        ));
    }
    // A throwaway master key: 32 bytes from two random v4 UUIDs.
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let log = log_file(dir, "gateway.log")?;
    let child = Command::new(&options.gateway_bin)
        .env_remove("ROUTER_API_KEY")
        .env_remove("ROUTER_DASHBOARD_PASSWORD")
        // The harness configures the throwaway instance through the dashboard.
        .env("ROUTER_DASHBOARD_AUTH", "disabled")
        .env("ROUTER_MASTER_KEY", STANDARD.encode(&key))
        .env(
            "ROUTER_DATABASE_URL",
            format!("sqlite://{}", dir.join("stress.db").display()),
        )
        .env("ROUTER_BIND", format!("127.0.0.1:{port}"))
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("cannot start gateway: {e}"))?;
    Ok(ChildGuard(child))
}

async fn wait_ready(client: &reqwest::Client, base: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(response) = client.get(format!("{base}/readyz")).send().await
            && response.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("gateway did not become ready within 30 s (see gateway.log)".into())
}

// ---------------------------------------------------------------- dashboard setup

/// Drives the dashboard over HTTP exactly like a browser form submission.
struct Dashboard {
    client: reqwest::Client,
    base: String,
    csrf: String,
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

fn encode(fields: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid").expect("static URL");
    for (name, value) in fields {
        url.query_pairs_mut().append_pair(name, value);
    }
    url.query().unwrap_or_default().to_owned()
}

impl Dashboard {
    async fn connect(base: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let mut dashboard = Self {
            client,
            base: base.to_owned(),
            csrf: String::new(),
        };
        let page = dashboard.page("/providers/new").await?;
        dashboard.csrf = between(&page, "name=\"_csrf\" value=\"", "\"")
            .ok_or("dashboard page had no CSRF token")?
            .to_owned();
        Ok(dashboard)
    }

    async fn page(&self, path: &str) -> Result<String, String> {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("GET {path} returned {}", response.status()));
        }
        response.text().await.map_err(|e| e.to_string())
    }

    /// Submits a form and returns the redirect target, if any.
    async fn submit(&self, path: &str, fields: &[(&str, &str)]) -> Result<String, String> {
        let mut all = vec![("_csrf", self.csrf.as_str())];
        all.extend_from_slice(fields);
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, &self.base)
            .body(encode(&all))
            .send()
            .await
            .map_err(|e| format!("POST {path}: {e}"))?;
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if status.is_success() || status.is_redirection() {
            Ok(location)
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(format!("POST {path} returned {status}: {}", body.trim()))
        }
    }

    /// Custom provider + one enabled connection whose model list is refreshed.
    async fn provider(&self, slug: &str, base_url: &str) -> Result<String, String> {
        let name = format!("Stress {slug}");
        let group = self
            .submit(
                "/providers/custom",
                &[
                    ("name", &name),
                    ("slug", slug),
                    ("base_url", base_url),
                    ("protocol", "openai"),
                ],
            )
            .await?;
        self.submit(
            &group,
            &[
                ("name", "load"),
                ("base_url", base_url),
                ("protocol", "openai"),
                ("enabled", "1"),
                ("api_key", "mock-upstream-key"),
            ],
        )
        .await?;
        let page = self.page(&group).await?;
        let connection = page
            .split("href=\"/providers/")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .find(|id| id.len() == 36 && !id.contains('/'))
            .ok_or("connection ID not found on the provider page")?
            .to_owned();
        self.submit(&format!("/providers/{connection}/models"), &[])
            .await?;
        Ok(connection)
    }
}

struct Target {
    base: String,
    api_key: String,
    combo_model: String,
    direct_model: String,
}

async fn configure(
    base: &str,
    mock_port: u16,
    logging: Option<&str>,
    history_buffer: Option<&str>,
) -> Result<Target, String> {
    let dashboard = Dashboard::connect(base).await?;
    let primary = dashboard
        .provider(
            "stress-primary",
            &format!("http://127.0.0.1:{mock_port}/primary/v1"),
        )
        .await?;
    let backup = dashboard
        .provider(
            "stress-backup",
            &format!("http://127.0.0.1:{mock_port}/backup/v1"),
        )
        .await?;
    let combo = dashboard
        .submit(
            "/combos",
            &[
                ("name", "stress"),
                ("strategy", "priority"),
                ("category", "text"),
                ("affinity_mode", "off"),
                ("sticky_minutes", "5"),
            ],
        )
        .await?;
    for connection in [&primary, &backup] {
        dashboard
            .submit(
                &format!("{combo}/steps"),
                &[("provider_id", connection), ("model", MOCK_MODEL)],
            )
            .await?;
    }
    if let Some(mode) = logging {
        dashboard
            .submit(
                "/settings/logging",
                &[("logging_mode", mode), ("confirm_extended", "yes")],
            )
            .await?;
    }
    if let Some(scale) = history_buffer {
        dashboard
            .submit("/settings/history-buffer", &[("history_buffer", scale)])
            .await?;
    }
    let key = dashboard
        .submit(
            "/api-keys",
            &[("name", "stress"), ("access", "all"), ("expiry", "never")],
        )
        .await?;
    let reveal = dashboard.page(&key).await?;
    let api_key = between(&reveal, "class=\"key-secret\">", "<")
        .ok_or("generated API key was not revealed")?
        .to_owned();
    Ok(Target {
        base: base.to_owned(),
        api_key,
        combo_model: "combo/stress".into(),
        direct_model: format!("stress-primary/{MOCK_MODEL}"),
    })
}

// ---------------------------------------------------------------- traffic

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Models,
    Json,
    Stream,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Outcome {
    Ok,
    Http(u16),
    Timeout,
    Connect,
    Incomplete,
    Invalid,
}

impl Outcome {
    fn label(&self) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::Http(status) => format!("http_{status}"),
            Self::Timeout => "timeout".into(),
            Self::Connect => "connect".into(),
            Self::Incomplete => "stream_incomplete".into(),
            Self::Invalid => "invalid_body".into(),
        }
    }
}

struct Sample {
    kind: Kind,
    direct: bool,
    outcome: Outcome,
    latency: Duration,
    ttfb: Option<Duration>,
    bytes: u64,
}

const FILLER: &str = "The quarterly report covers revenue, churn, hiring plans and the \
infrastructure migration. Summarize risks, list open questions and suggest next steps. ";

fn text(rng: &mut Rng) -> String {
    // Mostly short chat turns, some pasted documents.
    let chars = match rng.below(10) {
        0..=5 => 120 + rng.below(400),
        6..=8 => 1_500 + rng.below(2_500),
        _ => 8_000 + rng.below(8_000),
    } as usize;
    FILLER.chars().cycle().take(chars).collect()
}

fn chat_body(rng: &mut Rng, model: &str, stream: bool, user: usize) -> Value {
    let mut messages = vec![json!({"role": "system", "content": "You are a concise assistant."})];
    for _ in 0..rng.below(4) {
        messages.push(json!({"role": "user", "content": text(rng)}));
        messages.push(json!({"role": "assistant", "content": "Here is a short answer."}));
    }
    messages.push(json!({"role": "user", "content": text(rng)}));
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": 256,
        "temperature": 0.7,
        "user": format!("virtual-user-{user}"),
        "stream": stream,
    });
    if stream {
        body["stream_options"] = json!({"include_usage": true});
    }
    body
}

fn transport(error: &reqwest::Error) -> Outcome {
    if error.is_timeout() {
        Outcome::Timeout
    } else if error.is_connect() {
        Outcome::Connect
    } else {
        Outcome::Incomplete
    }
}

struct Load {
    client: reqwest::Client,
    target: Target,
    options: Options,
}

async fn send(load: &Load, rng: &mut Rng, user: usize) -> Sample {
    let options = &load.options;
    let target = &load.target;
    let started = Instant::now();
    let (kind, direct) = if rng.unit() < options.models_ratio {
        (Kind::Models, false)
    } else if rng.unit() < options.stream_ratio {
        (Kind::Stream, rng.unit() < options.direct_ratio)
    } else {
        (Kind::Json, rng.unit() < options.direct_ratio)
    };
    let request = if kind == Kind::Models {
        load.client.get(format!("{}/v1/models", target.base))
    } else {
        let model = if direct {
            &target.direct_model
        } else {
            &target.combo_model
        };
        load.client
            .post(format!("{}/v1/chat/completions", target.base))
            .json(&chat_body(rng, model, kind == Kind::Stream, user))
    };
    let run = async {
        let response = match request.bearer_auth(&target.api_key).send().await {
            Ok(response) => response,
            Err(error) => return (transport(&error), None, 0),
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let bytes = response.bytes().await.map_or(0, |b| b.len() as u64);
            return (Outcome::Http(status), None, bytes);
        }
        if kind != Kind::Stream {
            return match response.bytes().await {
                Err(error) => (transport(&error), None, 0),
                Ok(body) => {
                    let valid = serde_json::from_slice::<Value>(&body).is_ok_and(|value| {
                        value["choices"].as_array().is_some_and(|c| !c.is_empty())
                            || value["data"].is_array()
                    });
                    let outcome = if valid { Outcome::Ok } else { Outcome::Invalid };
                    (outcome, None, body.len() as u64)
                }
            };
        }
        let mut stream = response.bytes_stream();
        let (mut first, mut bytes, mut tail) = (None, 0u64, Vec::<u8>::new());
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    first.get_or_insert_with(|| started.elapsed());
                    bytes += chunk.len() as u64;
                    tail.extend_from_slice(&chunk);
                    let keep = tail.len().saturating_sub(32);
                    tail.drain(..keep);
                }
                Err(error) => return (transport(&error), first, bytes),
            }
        }
        // A committed stream must end with the terminator, never silently.
        let done = tail.windows(12).any(|w| w == b"data: [DONE]");
        let outcome = if done {
            Outcome::Ok
        } else {
            Outcome::Incomplete
        };
        (outcome, first, bytes)
    };
    let (outcome, ttfb, bytes) =
        tokio::time::timeout(options.timeout, run)
            .await
            .unwrap_or((Outcome::Timeout, None, 0));
    Sample {
        kind,
        direct,
        outcome,
        latency: started.elapsed(),
        ttfb,
        bytes,
    }
}

/// One closed-loop virtual user. Only requests started inside the measured
/// window are kept; requests in flight at the end are allowed to finish.
async fn virtual_user(
    load: Arc<Load>,
    user: usize,
    measure_from: Instant,
    stop_at: Instant,
) -> Vec<Sample> {
    let mut rng = Rng::seeded();
    let mut samples = Vec::new();
    while Instant::now() < stop_at {
        let started = Instant::now();
        let sample = send(&load, &mut rng, user).await;
        if started >= measure_from {
            samples.push(sample);
        }
        if !load.options.think.is_zero() {
            tokio::time::sleep(load.options.think).await;
        }
    }
    samples
}

// ---------------------------------------------------------------- /proc sampling

#[derive(Clone, Copy, Default)]
struct ProcSample {
    rss_kib: u64,
    threads: u64,
    fds: u64,
    cpu_ticks: u64,
}

fn read_proc(pid: u32) -> Option<ProcSample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized command name; utime/stime are 14 and 15.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |index: usize| fields.get(index).and_then(|v| v.parse::<u64>().ok());
    Some(ProcSample {
        rss_kib: field("VmRSS:"),
        threads: field("Threads:"),
        fds: std::fs::read_dir(format!("/proc/{pid}/fd")).map_or(0, |dir| dir.count() as u64),
        cpu_ticks: ticks(11)? + ticks(12)?,
    })
}

fn clock_ticks() -> f64 {
    Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(100.0)
}

#[derive(Default)]
struct ProcSummary {
    rss_avg_mib: f64,
    rss_peak_mib: f64,
    cpu_cores: f64,
    threads_peak: u64,
    fds_peak: u64,
}

/// Samples a process every 250 ms until `stop` is set.
async fn monitor(pid: u32, stop: Arc<AtomicBool>, ticks_per_sec: f64) -> Option<ProcSummary> {
    let started = Instant::now();
    let first = read_proc(pid)?;
    let mut samples = vec![first];
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Some(sample) = read_proc(pid) {
            samples.push(sample);
        }
    }
    let last = *samples.last()?;
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let mib = |kib: u64| kib as f64 / 1024.0;
    Some(ProcSummary {
        rss_avg_mib: mib(samples.iter().map(|s| s.rss_kib).sum::<u64>() / samples.len() as u64),
        rss_peak_mib: mib(samples.iter().map(|s| s.rss_kib).max()?),
        cpu_cores: (last.cpu_ticks - first.cpu_ticks) as f64 / ticks_per_sec / elapsed,
        threads_peak: samples.iter().map(|s| s.threads).max()?,
        fds_peak: samples.iter().map(|s| s.fds).max()?,
    })
}

// ---------------------------------------------------------------- reporting

fn percentile(sorted: &[Duration], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1].as_secs_f64() * 1000.0
}

fn latency_json(samples: &[&Sample]) -> Value {
    let mut latencies: Vec<Duration> = samples.iter().map(|s| s.latency).collect();
    latencies.sort_unstable();
    let ok = samples.iter().filter(|s| s.outcome == Outcome::Ok).count();
    json!({
        "requests": samples.len(),
        "success_pct": if samples.is_empty() { 100.0 } else { 100.0 * ok as f64 / samples.len() as f64 },
        "p50_ms": percentile(&latencies, 0.50),
        "p99_ms": percentile(&latencies, 0.99),
    })
}

struct Level {
    users: usize,
    report: Value,
    passed: bool,
}

fn summarize(
    users: usize,
    samples: &[Sample],
    window: Duration,
    gateway: Option<&ProcSummary>,
    loadgen: Option<&ProcSummary>,
    options: &Options,
) -> Level {
    let total = samples.len();
    let ok = samples.iter().filter(|s| s.outcome == Outcome::Ok).count();
    let success = if total == 0 {
        0.0
    } else {
        100.0 * ok as f64 / total as f64
    };
    let mut latencies: Vec<Duration> = samples.iter().map(|s| s.latency).collect();
    latencies.sort_unstable();
    let mut ttfb: Vec<Duration> = samples.iter().filter_map(|s| s.ttfb).collect();
    ttfb.sort_unstable();
    let mut errors = std::collections::BTreeMap::<String, usize>::new();
    for sample in samples.iter().filter(|s| s.outcome != Outcome::Ok) {
        *errors.entry(sample.outcome.label()).or_default() += 1;
    }
    let secs = window.as_secs_f64();
    let p99 = percentile(&latencies, 0.99);
    let passed = total > 0 && success >= options.slo_success && p99 <= options.slo_p99_ms;
    let pick = |f: &dyn Fn(&Sample) -> bool| samples.iter().filter(|s| f(s)).collect::<Vec<_>>();
    let proc_json = |summary: Option<&ProcSummary>| {
        summary.map(|p| {
            json!({
                "rss_avg_mib": p.rss_avg_mib, "rss_peak_mib": p.rss_peak_mib,
                "cpu_cores": p.cpu_cores, "threads_peak": p.threads_peak, "open_fds_peak": p.fds_peak,
            })
        })
    };
    let report = json!({
        "users": users,
        "measured_secs": secs,
        "requests": total,
        "throughput_rps": total as f64 / secs,
        "success_rate_pct": success,
        "errors": errors,
        "latency_ms": {
            "mean": if total == 0 { 0.0 } else {
                latencies.iter().map(Duration::as_secs_f64).sum::<f64>() * 1000.0 / total as f64
            },
            "p50": percentile(&latencies, 0.50),
            "p90": percentile(&latencies, 0.90),
            "p95": percentile(&latencies, 0.95),
            "p99": p99,
            "p999": percentile(&latencies, 0.999),
            "max": percentile(&latencies, 1.0),
        },
        "stream_ttfb_ms": {
            "p50": percentile(&ttfb, 0.50),
            "p99": percentile(&ttfb, 0.99),
        },
        "received_mib_per_sec": samples.iter().map(|s| s.bytes).sum::<u64>() as f64 / 1_048_576.0 / secs,
        "by_kind": {
            "combo_json": latency_json(&pick(&|s| s.kind == Kind::Json && !s.direct)),
            "combo_stream": latency_json(&pick(&|s| s.kind == Kind::Stream && !s.direct)),
            "direct_json": latency_json(&pick(&|s| s.kind == Kind::Json && s.direct)),
            "direct_stream": latency_json(&pick(&|s| s.kind == Kind::Stream && s.direct)),
            "models": latency_json(&pick(&|s| s.kind == Kind::Models)),
        },
        "gateway_process": proc_json(gateway),
        "load_generator_process": proc_json(loadgen),
        "slo_passed": passed,
    });
    Level {
        users,
        report,
        passed,
    }
}

fn num(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

fn print_header() {
    println!(
        "\n{:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9} {:>9} {:>6} {:>6} {:>9}  errors",
        "users",
        "req/s",
        "success",
        "p50 ms",
        "p95 ms",
        "p99 ms",
        "max ms",
        "ttfb p99",
        "RSS MiB",
        "CPU",
        "fds",
        "hist drop"
    );
}

fn print_level(level: &Level) {
    let r = &level.report;
    let gateway = &r["gateway_process"];
    let errors = r["errors"]
        .as_object()
        .map(|errors| {
            errors
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    println!(
        "{:>6} {:>8.1} {:>7.2}% {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>9.0} {:>9.1} {:>6.2} {:>6} {:>9}  {}{}",
        level.users,
        num(&r["throughput_rps"]),
        num(&r["success_rate_pct"]),
        num(&r["latency_ms"]["p50"]),
        num(&r["latency_ms"]["p95"]),
        num(&r["latency_ms"]["p99"]),
        num(&r["latency_ms"]["max"]),
        num(&r["stream_ttfb_ms"]["p99"]),
        num(&gateway["rss_peak_mib"]),
        num(&gateway["cpu_cores"]),
        gateway["open_fds_peak"].as_u64().unwrap_or(0),
        r["history_writes_dropped"].as_u64().unwrap_or(0),
        if errors.is_empty() { "-" } else { &errors },
        if level.passed {
            ""
        } else {
            "   << misses target"
        },
    );
}

/// Gateway warnings logged so far: all `WARN` lines, and those reporting a
/// request-history write or finalization that was abandoned.
fn log_warnings(log: Option<&Path>) -> (usize, usize) {
    let Some(text) = log.and_then(|path| std::fs::read_to_string(path).ok()) else {
        return (0, 0);
    };
    let warn = text.lines().filter(|line| line.contains(" WARN ")).count();
    let history = text
        .lines()
        .filter(|line| {
            line.contains("request history write unavailable")
                || line.contains("request history finalization unavailable")
        })
        .count();
    (warn, history)
}

// ---------------------------------------------------------------- orchestration

async fn run_level(
    load: &Arc<Load>,
    users: usize,
    gateway_pid: Option<u32>,
    log: Option<&Path>,
    ticks: f64,
) -> Level {
    let options = &load.options;
    let warnings_before = log_warnings(log);
    let measure_from = Instant::now() + options.warmup;
    let stop_at = measure_from + options.duration;
    let tasks: Vec<_> = (0..users)
        .map(|user| tokio::spawn(virtual_user(load.clone(), user, measure_from, stop_at)))
        .collect();
    tokio::time::sleep_until(measure_from.into()).await;
    let stop = Arc::new(AtomicBool::new(false));
    let gateway_monitor = gateway_pid.map(|pid| tokio::spawn(monitor(pid, stop.clone(), ticks)));
    let loadgen_monitor = tokio::spawn(monitor(std::process::id(), stop.clone(), ticks));
    tokio::time::sleep_until(stop_at.into()).await;
    stop.store(true, Ordering::Relaxed);
    let gateway = match gateway_monitor {
        Some(task) => task.await.ok().flatten(),
        None => None,
    };
    let loadgen = loadgen_monitor.await.ok().flatten();
    let mut samples = Vec::new();
    for task in tasks {
        samples.extend(task.await.unwrap_or_default());
    }
    // Let the history writer drain its queue (or log drops) before counting.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let warnings_after = log_warnings(log);
    let mut level = summarize(
        users,
        &samples,
        options.duration,
        gateway.as_ref(),
        loadgen.as_ref(),
        options,
    );
    level.report["gateway_warnings"] = json!(warnings_after.0 - warnings_before.0);
    level.report["history_writes_dropped"] = json!(warnings_after.1 - warnings_before.1);
    level
}

fn run_dir(options: &Options) -> Result<PathBuf, String> {
    let dir = options.out.clone().unwrap_or_else(|| {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("target/stress/{secs}"))
    });
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

async fn run(options: Options) -> Result<(), String> {
    let dir = run_dir(&options)?;
    let ticks = clock_ticks();
    // Each virtual user may hold a client connection; keep idle ones for reuse.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(usize::MAX)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    // Guards live until the end of this function, then stop both children.
    let mut _children = Vec::new();
    let (target, gateway_pid, gateway_log) = match &options.external {
        Some(external) => {
            println!(
                "Target: external gateway {} (model {})",
                external.url, external.model
            );
            println!("Warning: requests go to whatever upstream that gateway uses.");
            let target = Target {
                base: external.url.clone(),
                api_key: external.api_key.clone(),
                combo_model: external.model.clone(),
                direct_model: external.model.clone(),
            };
            (target, external.pid, None)
        }
        None => {
            let (mock, mock_port) = spawn_mock(&options.mock, &dir)?;
            let port = free_port()?;
            let gateway = spawn_gateway(&options, &dir, port)?;
            let pid = gateway.0.id();
            _children.push(mock);
            _children.push(gateway);
            let base = format!("http://127.0.0.1:{port}");
            wait_ready(&client, &base).await?;
            println!(
                "Gateway {} (pid {pid}) at {base}",
                options.gateway_bin.display()
            );
            println!(
                "Mock upstream at 127.0.0.1:{mock_port}; run files in {}",
                dir.display()
            );
            let target = configure(
                &base,
                mock_port,
                options.logging.as_deref(),
                options.history_buffer.as_deref(),
            )
            .await?;
            println!(
                "Configured through the dashboard: 2 providers, combo/stress (primary -> backup), 1 API key"
            );
            (target, Some(pid), Some(dir.join("gateway.log")))
        }
    };
    let load = Arc::new(Load {
        client,
        target,
        options,
    });
    let options = &load.options;

    // Smoke test every request shape once before measuring anything.
    let mut rng = Rng::seeded();
    for _ in 0..8 {
        let sample = send(&load, &mut rng, 0).await;
        if sample.outcome != Outcome::Ok && options.mock.error_rate == 0.0 {
            let hint = if options.external.is_some() {
                "check --url, --api-key and --model".to_owned()
            } else {
                format!("check gateway.log in {}", dir.display())
            };
            return Err(format!(
                "smoke request failed ({}); {hint}",
                sample.outcome.label()
            ));
        }
    }
    let baseline = gateway_pid.and_then(read_proc);
    if let Some(base) = baseline {
        println!(
            "Idle gateway before load: {:.1} MiB RSS, {} threads",
            base.rss_kib as f64 / 1024.0,
            base.threads
        );
    }
    let m = &options.mock;
    println!(
        "Workload: {:.0}% streaming, {:.0}% direct IDs, {:.0}% /v1/models; upstream ttfb {}±{} ms, \
         {} chunks x {} ms, error rate {:.1}%; {} s warm-up + {} s per level",
        options.stream_ratio * 100.0,
        options.direct_ratio * 100.0,
        options.models_ratio * 100.0,
        m.ttfb_ms,
        m.jitter_ms,
        m.chunks,
        m.chunk_ms,
        m.error_rate * 100.0,
        options.warmup.as_secs(),
        options.duration.as_secs()
    );
    println!(
        "Target: p99 <= {} ms and success >= {}%",
        options.slo_p99_ms, options.slo_success
    );

    print_header();
    let mut levels = Vec::new();
    for &users in &options.levels {
        let level = run_level(&load, users, gateway_pid, gateway_log.as_deref(), ticks).await;
        print_level(&level);
        let stop = !level.passed && !options.keep_going;
        levels.push(level);
        if stop {
            break;
        }
    }

    tokio::time::sleep(Duration::from_secs(5)).await;
    let after = gateway_pid.and_then(read_proc);
    let healthy = load
        .client
        .get(format!("{}/readyz", load.target.base))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());

    let mut summary = String::new();
    let best = levels.iter().filter(|l| l.passed).max_by_key(|l| l.users);
    let peak = levels.iter().max_by(|a, b| {
        num(&a.report["throughput_rps"]).total_cmp(&num(&b.report["throughput_rps"]))
    });
    let _ = writeln!(summary, "\nSummary");
    match best {
        Some(level) if levels.iter().all(|l| l.passed) => {
            let _ = writeln!(
                summary,
                "  All levels met the target; at least {} simultaneous users. Try higher --levels.",
                level.users
            );
        }
        Some(level) => {
            let _ = writeln!(
                summary,
                "  Max simultaneous users within target: {}",
                level.users
            );
        }
        None => {
            let _ = writeln!(summary, "  No level met the target.");
        }
    }
    if gateway_log.is_some() {
        let dropped = |l: &&Level| l.report["history_writes_dropped"].as_u64().unwrap_or(0) > 0;
        match levels.iter().find(dropped) {
            None => {
                let _ = writeln!(
                    summary,
                    "  Request history kept every write at every level."
                );
            }
            Some(first) => {
                let _ = writeln!(
                    summary,
                    "  Request history dropped writes from {} users on (see gateway.log); \
                     responses were unaffected but history/usage totals are incomplete.",
                    first.users
                );
            }
        }
    }
    if let Some(level) = peak {
        let _ = writeln!(
            summary,
            "  Peak throughput: {:.1} req/s at {} users",
            num(&level.report["throughput_rps"]),
            level.users
        );
    }
    if let (Some(base), Some(level)) = (baseline, levels.last()) {
        let peak_mib = num(&level.report["gateway_process"]["rss_peak_mib"]);
        let base_mib = base.rss_kib as f64 / 1024.0;
        let _ = writeln!(
            summary,
            "  Gateway memory: {base_mib:.1} MiB idle -> {peak_mib:.1} MiB peak at {} users (~{:.0} KiB per user)",
            level.users,
            (peak_mib - base_mib).max(0.0) * 1024.0 / level.users as f64
        );
    }
    if let Some(after) = after {
        let _ = writeln!(
            summary,
            "  After 5 s idle: {:.1} MiB RSS, {} threads, {} open files",
            after.rss_kib as f64 / 1024.0,
            after.threads,
            after.fds
        );
    }
    let _ = writeln!(
        summary,
        "  Gateway readiness after load: {}",
        if healthy { "ready" } else { "NOT READY" }
    );
    let loadgen_cpu = levels
        .iter()
        .map(|l| num(&l.report["load_generator_process"]["cpu_cores"]))
        .fold(0.0, f64::max);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let _ = writeln!(
        summary,
        "  Load generator used up to {loadgen_cpu:.2} of {cores} cores on the same machine as the gateway and mock;"
    );
    let _ = writeln!(
        summary,
        "  if that is near the total, results are limited by this machine, not the gateway."
    );
    print!("{summary}");

    let report = json!({
        "options": {
            "levels": options.levels,
            "duration_secs": options.duration.as_secs(),
            "warmup_secs": options.warmup.as_secs(),
            "think_ms": options.think.as_millis() as u64,
            "stream_ratio": options.stream_ratio,
            "direct_ratio": options.direct_ratio,
            "models_ratio": options.models_ratio,
            "slo_p99_ms": options.slo_p99_ms,
            "slo_success_pct": options.slo_success,
            "upstream": {"ttfb_ms": m.ttfb_ms, "jitter_ms": m.jitter_ms, "chunks": m.chunks,
                         "chunk_ms": m.chunk_ms, "error_rate": m.error_rate},
            "external": options.external.is_some(),
        },
        "gateway_idle_rss_mib": baseline.map(|b| b.rss_kib as f64 / 1024.0),
        "gateway_rss_after_mib": after.map(|a| a.rss_kib as f64 / 1024.0),
        "gateway_ready_after": healthy,
        "max_users_within_target": best.map(|l| l.users),
        "levels": levels.iter().map(|l| l.report.clone()).collect::<Vec<_>>(),
    });
    let path = dir.join("report.json");
    let pretty = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(&path, pretty).map_err(|e| e.to_string())?;
    println!("  Report: {}", path.display());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let mock_mode = args.first().is_some_and(|a| a == "mock");
    let mut rest = if mock_mode { &args[1..] } else { &args[..] }.to_vec();
    let mut mock_port = 0;
    if mock_mode && let Some(index) = rest.iter().position(|a| a == "--port") {
        let Some(port) = rest.get(index + 1).and_then(|p| p.parse().ok()) else {
            eprintln!("--port needs a number");
            return ExitCode::FAILURE;
        };
        mock_port = port;
        rest.drain(index..index + 2);
    }
    let options = match parse_options(&rest) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let work = async {
            if mock_mode {
                run_mock(options.mock, mock_port).await
            } else {
                run(options).await
            }
        };
        tokio::select! {
            result = work => result,
            _ = tokio::signal::ctrl_c() => Err("interrupted".into()),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
