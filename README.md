# SparkRouter

A small self-hosted LLM API gateway: one Rust executable, SQLite, and a server-rendered dashboard. Add providers, arrange provider/model pairs into ordered **combos**, and call them using the OpenAI Chat Completions API.

## Documentation

- [Build and deployment](BUILD.md)
- [Architecture and security boundaries](ARCHITECURE.md)
- [Testing and manual acceptance](TESTING.md)
- [Contributor and coding-agent guide](AGENTS.md)

## Run

Install Rust 1.88+ and a C toolchain (SQLx builds bundled SQLite). Build with:

```sh
cargo build --release --locked
export ROUTER_MASTER_KEY="$(openssl rand -base64 32)"
export ROUTER_DATABASE_URL="sqlite://router.db"
# Optional authentication for /v1/*:
export ROUTER_API_KEY="a-long-random-gateway-key"
./target/release/sparkrouter
```

PowerShell:

```powershell
cargo build --release --locked
$bytes = [byte[]]::new(32)
[System.Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
$env:ROUTER_MASTER_KEY = [Convert]::ToBase64String($bytes)
$env:ROUTER_DATABASE_URL = 'sqlite://router.db'
$env:ROUTER_API_KEY = 'a-long-random-gateway-key'
.\target\release\sparkrouter.exe
```

**Save the generated master key securely and reuse it on every restart.** Changing it does not rotate stored credentials; existing credentials become unreadable. Back up the key separately from the database. The executable reads process environment variables; `.env.example` is a reference, not an automatically loaded configuration file.

Open **http://127.0.0.1:8080/providers**:

Use the left sidebar to switch between **Providers** and **Combos**.
The top-right theme button switches between **Light** and **Dark**. Initially it
matches your device's theme; after toggling, your choice is saved in your browser.
Dashboard icons use locally embedded Lucide SVGs, with no icon CDN or external
font dependency. Their license is included at `/static/icons-LICENSE.txt`.

1. Select the **OpenRouter** provider tile, click **Add connection**, give the connection a name (such as
   `Personal`), and add its API key. Add more named connections on the same page.
2. Select another preset, such as **Groq**, or **Custom provider** to create a
   named provider with a base URL and protocol. This creates an empty card;
   open it to add multiple named API-key connections. Custom providers remain
   separate even when their URLs match. Existing custom connections are
   imported into provider groups without changing keys or combo references.
3. Open **Combos**, create `coding`, and add provider/model steps in preference order.
4. Choose a specific **provider / connection** for each step and use the up/down
   buttons to change fallback order. Enter model IDs exactly as your upstream
   accepts them. Keys within a provider are not automatically rotated or retried.

Choose **Priority**, **Round robin**, or **Random** when creating or editing a
combo. Existing combos default to Priority. The Combos list shows its strategy.

The Combos list has a **Copy** button beside each name for its `combo/<name>`
model ID. **Duplicate** creates an independent combo with the same ordered steps
and opens it for editing. Copies are named `<name>-copy`, then `<name>-copy-2`,
and so on; long names are shortened to keep the 64-character limit. Existing
connections are reused, not duplicated, and the selected strategy is preserved.

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"combo/coding","messages":[{"role":"user","content":"Hello"}]}'

# Streaming: add "stream":true to the JSON and use curl -N.
curl http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer $ROUTER_API_KEY"
```

Set an OpenAI-compatible client's base URL to `http://127.0.0.1:8080/v1` and model to `combo/coding`. Generate a key under **API keys**, or set `ROUTER_API_KEY`. Either key can call every combo. If neither is configured, omit Authorization (clients requiring a key may send any placeholder).

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `ROUTER_MASTER_KEY` | **required** | Base64-encoded 32 random bytes; AES-256-GCM master key |
| `ROUTER_DATABASE_URL` | `sqlite://router.db` | SQLite URL; database and schema created on startup |
| `ROUTER_API_KEY` | unset | Optional bearer key protecting `/v1/*`; an explicitly empty value is rejected. Dashboard-generated keys also authorize `/v1/*` and, once any exist, require authentication even when this variable is unset |
| `ROUTER_BIND` | `127.0.0.1:8080` | IP address and port |
| `RUST_LOG` | `info` | `tracing` filter |

The parent database directory must already exist and be writable. SQLite WAL/SHM sidecar files are normal while running. For backups, stop the application first or use SQLite's online backup facility rather than copying an active database file alone.

## Providers and protocols

Providers are configuration, not vendor-specific implementations. The dashboard includes OmniRoute's API-key chat presets that use the **OpenAI** or **Anthropic** adapters, including OpenRouter, Groq, DeepSeek, Together, and Fireworks. Local OpenAI-compatible servers use the **OpenAI** protocol too. Include any required API prefix (usually `/v1`) in the base URL. Trailing slashes are normalized; URL credentials, query strings, and fragments are rejected. HTTP is supported for local servers. An empty API key is allowed for unauthenticated local servers.

The **Anthropic** protocol uses `https://api.anthropic.com/v1` and translates Chat Completions to `/messages`, including text, system instructions, tools, and streaming. Vendor-specific protocol limitations are described below.

The connection test sends `GET {base_url}/models` with the saved credentials. It verifies that endpoint, not model access or generation. **Refresh models** on a connection stores that list. Open the model dropdown on the connection to search the returned names and fields; the combo editor suggests those IDs, and you can still type any model. Some compatible servers do not implement model listing; a failed refresh does not mean their chat endpoint is unusable. Save changes before testing them.

Saved API keys are never rendered back into HTML. Leaving the key field blank
when editing a connection preserves the existing encrypted value. To clear a
saved key, recreate the connection. Connections referenced by combos cannot be
deleted until those steps are removed; disable them to temporarily skip them.
Custom provider cards carry a **Custom** badge. Open a custom provider and choose
**Delete provider** to remove it and all its connections. Deletion is blocked if
any connection is referenced by a combo; preset provider tiles are not removable.

## Routing and streaming

- **Priority** (default) starts at the first enabled step, then follows configured order on retryable failures.
- **Round robin** rotates the starting step across enabled steps, once per request. After the last step it returns to the first. Cursors are per combo, shared across concurrent requests in one process, and reset on restart; changes to enabled steps may change the next starting step.
- **Random** chooses a uniformly random enabled starting step for each request. Repeated selections are normal; it does not guarantee equal traffic counts.
- Round robin and Random fall back cyclically through the remaining enabled steps in configured order. Each step is attempted at most once per request. Disabled providers never consume a turn. Permanent errors stop all strategies; streaming cannot switch after commitment.
- Connect/read/request failures and HTTP **408, 429, 500, 502, 503, 504** try the next step. The status policy lives in `src/routing/combo.rs`.
- Other HTTP failures, including **400, 401, 403, 404, 422**, stop the chain. No repeated attempts, cooldown state, or background work.
- Empty combos, entirely disabled combos, or exhausted chains return a sanitized OpenAI-shaped `502` error. Unknown combo names return `404`.
- OpenAI requests remain `serde_json::Value`; only the model is replaced for each target. Unknown fields are preserved by the OpenAI adapter.
- **Failover is possible only before a streaming response is committed.** The adapter obtains initial upstream data before returning the response. After that, the selected upstream owns the request. A mid-stream failure terminates the HTTP body with an error; it never switches to another provider or restarts generated text. Clients must handle incomplete streams.
- One shared pooled HTTP client uses a 10-second connect timeout, 120-second idle read timeout, and 90-second idle pool lifetime. Each attempt has 120 seconds to prepare its response; the entire pre-commit fallback phase has a 180-second ceiling. Committed streams can run longer while data continues to arrive. Redirects are rejected to protect credentials.
- Requests are limited to 4 MiB, non-stream responses to 16 MiB, and SSE events to 1 MiB. Streaming is incremental and backpressured, without buffering a complete generation. A clean upstream EOF without the protocol's completion marker is treated as an interrupted stream.

## Security and deployment

Keys are encrypted using AES-256-GCM with a fresh random nonce and authenticated provider ID. The database stores versioned ciphertext; the master key is never stored there. Secret values are wrapped with `secrecy` and key/decryption buffers use `zeroize`. Normal logs include routing IDs, model, latency, status, and fallback events; they omit credentials and upstream bodies.

The dashboard has **no user accounts or login**. Keep it on localhost or put it behind an authenticated reverse proxy when exposing it on a network. `ROUTER_API_KEY` and dashboard-generated API keys protect gateway endpoints only. Generated keys are unrestricted, shown once, and stored as a master-key fingerprint rather than a recoverable secret. Dashboard forms have CSRF tokens and origin checks; scripts/styles are local embedded assets. Provider URLs intentionally permit private/local endpoints, so dashboard access grants control over outbound connections. TLS termination belongs at your reverse proxy.

The dashboard uses `Referrer-Policy: same-origin`: native form submissions retain their Origin for validation, and external destinations receive no referrer. If a submission is rejected, reload the form after a server restart (CSRF tokens change), use one consistent hostname and port, and ensure a reverse proxy preserves the browser-facing Host header.

### Docker

```sh
docker build -t sparkrouter .
docker volume create sparkrouter-data
docker run --rm --name sparkrouter \
  -p 127.0.0.1:8080:8080 \
  -v sparkrouter-data:/data \
  -e ROUTER_MASTER_KEY -e ROUTER_API_KEY \
  sparkrouter
```

The multi-stage image contains the release binary and a minimal Debian runtime with CA certificates, runs as an unprivileged user, and requires no Node.js. The container binds `0.0.0.0` internally; the example publishes only to host localhost. Bind-mounted directories must be writable by UID 10001.

## Architecture

One crate: Axum/Tokio HTTP server, Reqwest upstream client, SQLx/SQLite migrations, Askama templates, HTMX, and embedded CSS/JavaScript. `src/api` handles the public API, `src/routing` the fallback policy, `src/providers` protocol adapters, `src/db` persistence, and `src/web` dashboard routes. The protocol enum is the extension point for future adapters. There is no per-vendor class hierarchy. The display name is centralized in `src/lib.rs`.

## Development

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Tests use local mock HTTP servers and temporary SQLite databases; no real upstream keys are needed. Migrations and frontend assets are embedded at build time; no build-time database or frontend toolchain is required.

## MVP boundaries

Only `/v1/chat/completions` and `/v1/models` are implemented; requests route via combo names. Combos support Priority, Round robin, and Random starting-step selection with ordered fallback. No embeddings, Responses API, Gemini, OAuth, routing optimization, price optimization, queues, caching, telemetry platform, or background model discovery. Model IDs are arbitrary strings. To change a combo step's provider/model, remove it and add the replacement. Model listing includes configured combos even if empty or disabled.

Anthropic is a translation subset, not full OpenAI feature parity. It supports system/user/assistant/tool messages, text and images, function tools, and text/tool streaming. Multiple choices, developer/named messages, legacy functions, strict schemas, structured response formats, log probabilities, nonzero penalties, and other untranslatable generation options are rejected. Bookkeeping fields and image-detail hints are omitted; unknown extension fields pass through. Thinking blocks are not exposed as answer text. `max_tokens` defaults to 1024 if unspecified. Provider connection testing requires a `/models` endpoint. Streams idle for 120 seconds end as incomplete responses. There is no master-key rotation workflow or dashboard authentication in this version.
