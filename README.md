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

Open **http://127.0.0.1:8080/** for the Overview dashboard:

The Overview is the default login destination and sidebar entry. It shows SQLite read connectivity, successful and failed request counts, and current process resident memory (Linux; unavailable on other platforms). Status is sampled on page load; reload to refresh. Failed counts include errors, interrupted streams, and cancellations, not authentication/validation/traffic-limit rejections. Pending streams are excluded. Database/history failures show unavailable values instead of fabricated zeros; connectivity does not prove write access. It also summarizes retained request usage in five cards (requests, input/output/cache tokens, and USD spend) and ranks the top ten providers by requests. Use the sidebar to switch between **Overview**, **Providers**, **Combos**, **API keys**, **Test**, **History**, and **Settings**. Navigation wraps above the page on smaller screens.
The top-right theme button switches between **Light** and **Dark**. Initially it
matches your device's theme; after toggling, your choice is saved in your browser.
Dashboard icons use locally embedded Lucide SVGs, with no icon CDN or external
font dependency. Their license is included at `/static/icons-LICENSE.txt`.
The dashboard shares simple forms, buttons, and keyboard focus indicators across
pages. Navigation and page headings render on the server; connection and model
detail popovers use native HTML, with no JavaScript needed to open or close them.
JavaScript enhances partial updates, search, copy, step pickers, and test chat;
management forms still work with ordinary submissions when it is disabled.

1. Select the **OpenRouter** provider tile, click **Add connection**, give the connection a name (such as
   `Personal`), and add its API key. Add more named connections on the same page.
2. Select another preset, such as **Groq**, or **Custom provider** to create a
   named provider with a base URL and protocol. This creates an empty card;
   open it to add multiple named API-key connections. Custom providers remain
   separate even when their URLs match. Existing custom connections are
   imported into provider groups without changing keys or combo references.
3. Open **Combos**, create `coding`, and add provider/model steps in preference order.
4. Choose a **provider**, then one of its **connections**, for each step and use the up/down
   buttons to change fallback order. Enter model IDs exactly as your upstream
     accepts them. Each combo step targets one connection; it does not implicitly
     rotate or retry other keys within that provider.

Use **Edit** immediately before **Remove** to open a step's inline connection/model editor. Saving
keeps its ID and fallback position; refreshed model suggestions are optional.
Without JavaScript, use the **Edit step** disclosure below the row; all connection
options remain visible and ordinary form submissions still work.

Choose **Priority**, **Round robin**, or **Random** when creating or editing a
combo. Existing combos default to Priority. The Combos list shows its strategy.

The Combos list has a **Copy** button beside each name for its `combo/<name>`
model ID. **Duplicate** creates an independent combo with the same ordered steps
and opens it for editing. Copies are named `<name>-copy`, then `<name>-copy-2`,
and so on; long names are shortened to keep the 64-character limit. Existing
connections are reused, not duplicated, and the selected strategy is preserved.

Open **Test** (`/test`) for a chat-style workspace with a combo picker, user
message bubbles, and a single-line bottom composer with Send on its right.
Press Enter or click Send to send a message. **New chat** clears the conversation; **Routing attempts** expands
the latest request's diagnostic trace. Chat with a combo and inspect the actual routing attempts:
connection name, upstream model, configured step number, outcome, and latency.
Retryable failures and the selected target appear in request order, including
when a request fails. Tests use the same routing and round-robin state as API
requests and make real, potentially billable upstream calls. Enter a gateway
API key if authentication is configured; keys are not saved in browser storage.
Chat requires JavaScript, uses non-streaming responses, and keeps conversation
history only in the current page. Changing combos or clearing the chat starts a
fresh conversation.

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"combo/coding","messages":[{"role":"user","content":"Hello"}]}'

# Streaming: add "stream":true to the JSON and use curl -N.
curl http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer $ROUTER_API_KEY"
```

Set an OpenAI-compatible client's base URL to `http://127.0.0.1:8080/v1` and model to `combo/coding`. Generate a key under **API keys**, or set `ROUTER_API_KEY`. The environment key is unrestricted; generated keys can be unrestricted or scoped to selected model IDs. If neither is configured, omit Authorization (clients requiring a key may send any placeholder).

### Per-key traffic limits

Generated API keys optionally support **Requests per minute** (1–1,000,000) and
**Maximum concurrent requests** (1–10,000). Set these when generating a key or
under **Edit access / expiry / limits**; blank means unlimited. Existing keys
remain unlimited, as does `ROUTER_API_KEY`. Limits apply to chat completions and
test chat together; `/v1/models` is exempt.

Rate accounting uses fixed 60-second windows beginning with the first admitted
request for that key. Each admitted gateway request counts once, including
routing failures; fallback attempts do not consume additional quota. Invalid
boundary requests, denied models, and traffic-limit rejections do not consume
quota. A fixed window may allow a burst across its boundary; this is not a
rolling-window or token/spending budget.

Concurrent accounting covers upstream preparation and fallback. Successful
non-streaming requests release their slot when the handler finishes; streams
hold a slot until the response body finishes, errors, or is dropped. There is
no queue: exceeded limits return an OpenAI-shaped `429` with
`error.type: "rate_limit_error"` and `Retry-After` in seconds. Rate rejection
reports the remaining window time; concurrency rejection suggests one second,
not a guarantee that a slot will then be free. Rejections never call providers
or enter routed request history.

The API keys page shows configured limits, active requests, and rejection counts
in the current 60-second window; reload to refresh. Configuration persists in
SQLite, but counters are process-local and reset on restart. Multiple gateway
processes do not share counters. Editing limits does not reset existing counters
or cancel active requests; new admissions use the updated limits.

### Scoped gateway API keys

Choose **Only selected model IDs** when generating a key, then enter one exact
public ID per line, for example `combo/coding` or `openrouter/openai/gpt-6-luna`. Up to 200
IDs (up to 321 characters each) are allowed per key; IDs are case-sensitive, trimmed, and have no wildcard or
prefix semantics. Empty selected access deliberately permits no models. IDs need
not exist yet. An allowed combo authorizes its entire configured fallback chain,
but does not grant direct access to those steps' upstream models. Permissions
follow public names, not combo UUIDs: update the policy after renaming a combo;
reusing an allowed name grants access to the new combo under that name.

Optionally set expiry to **1–3650 days from now**. Expiration is stored as an
absolute timestamp and displayed in UTC. **Edit access / expiry** changes a key's
policy without rotating or revealing its secret; expiry is preserved unless you
explicitly remove or reset it. Existing keys remain unrestricted and non-expiring.

`/v1/models` lists only models allowed by the supplied key. A valid key calling a
disallowed model gets `403`; an expired, revoked, or invalid key gets `401` while
authentication is enabled. Test chat uses the same restrictions. Authorization
is checked at request start: expiry/revocation/policy edits do not interrupt
already-started requests or streams. Expired keys still keep authentication
enabled until explicitly revoked. Revoking the last key reopens the API only
when `ROUTER_API_KEY` is also unset.

A refreshed model can also be called directly using **`<provider-slug>/<upstream-model-id>`**, with no combo:

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"openrouter/openai/gpt-6-luna","messages":[{"role":"user","content":"Hello"}]}'
```

For example, `provider1/openai/gpt-6-luna` and `provider2/openai/gpt-6-luna`
are separate targets even when both providers list the same upstream model.
The gateway removes only the provider prefix before sending the request.

**Upgrade note:** old raw direct IDs are no longer aliases. Update client model
settings and exact API-key allowlists using `/v1/models` or the model-card Copy
button. Existing scopes are not automatically rewritten or broadened. Combos
and their upstream step model IDs are unchanged.

## Convenience scripts

Use `scripts/sparkrouter.sh` on Linux/macOS/WSL or `scripts/sparkrouter.ps1` on
Windows. Both offer `start`, `run`, `stop`, `restart`, `status`, `logs`, `health`,
`build`, `check`, `test`, and `keygen`. See [the helper guide](BUILD.md#convenience-scripts)
for setup and options.

```sh
# Explicitly load a prepared .env; never replace an existing master key.
bash scripts/sparkrouter.sh start --env-file .env
bash scripts/sparkrouter.sh health --env-file .env
bash scripts/sparkrouter.sh stop
```

```powershell
.\scripts\sparkrouter.ps1 start -EnvFile .env
.\scripts\sparkrouter.ps1 health -EnvFile .env
.\scripts\sparkrouter.ps1 stop
```

**Windows background stop is forced and interrupts active requests.** For
graceful Windows shutdown, use `run` instead of `start` and press Ctrl+C.
Scripts never load `.env` implicitly. The executable itself still reads only
process environment variables.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `ROUTER_MASTER_KEY` | **required** | Base64-encoded 32 random bytes; AES-256-GCM master key |
| `ROUTER_DATABASE_URL` | `sqlite://router.db` | SQLite URL; database and schema created on startup |
| `ROUTER_API_KEY` | unset | Optional bearer key protecting `/v1/*`; an explicitly empty value is rejected. Dashboard-generated keys also authorize `/v1/*` and, once any exist, require authentication even when this variable is unset |
| `ROUTER_BIND` | `127.0.0.1:8080` | IP address and port |
| `ROUTER_DASHBOARD_PASSWORD` | unset | Optional separate admin login; 16–1024 bytes, not blank. Use a long random password |
| `ROUTER_DASHBOARD_SECURE_COOKIE` | `false` | Set `true` for HTTPS deployment; marks dashboard session cookies Secure. Must be `true` or `false` |
| `RUST_LOG` | `info` | `tracing` filter |

The parent database directory must already exist and be writable. SQLite WAL/SHM sidecar files are normal while running. For backups, stop the application first or use SQLite's online backup facility rather than copying an active database file alone.

## Providers and protocols

Providers are configuration, not vendor-specific implementations. The dashboard includes OmniRoute's API-key chat presets that use the **OpenAI** or **Anthropic** adapters, including OpenRouter, Groq, DeepSeek, Together, and Fireworks. Local OpenAI-compatible servers use the **OpenAI** protocol too. Include any required API prefix (usually `/v1`) in the base URL. Trailing slashes are normalized; URL credentials, query strings, and fragments are rejected. HTTP is supported for local servers. An empty API key is allowed for unauthenticated local servers.

The **Anthropic** protocol uses `https://api.anthropic.com/v1` and translates Chat Completions to `/messages`, including text, system instructions, tools, and streaming. Vendor-specific protocol limitations are described below.

Provider detail pages show their **Routing slug**. Built-in defaults use the
OmniRoute catalog's routing aliases where available, such as `openrouter`,
`groq`, and `ds` (DeepSeek); dashboard preset URLs keep their existing names.
Custom providers may specify a slug at creation or leave it blank for a saved,
lowercase slug derived from their name. Slugs are 1–64 lowercase letters,
digits, or hyphens, without leading/trailing hyphens. Generated collisions get
`-2`, `-3`, etc. Preset names/aliases, `combo`, and `connection-` prefixes are
reserved. Existing custom providers receive saved slugs on startup without
changing credentials or combo references. Ungrouped non-preset connections use
a reserved connection-specific namespace.

The connection test sends `GET {base_url}/models` with the saved credentials. It verifies that endpoint, not model access or generation. **Refresh models** stores that list. Model cards show and copy the provider-prefixed **public ID** (Copy uses the modern Clipboard API on HTTPS/localhost, with a legacy fallback for plain HTTP LAN access); model details also show the **upstream ID** used by combo steps. Cards support search and show only a compact context-size tag (for example, `128k ctx` or `1.05M ctx`, rounded to at most two decimal places); output limits, pricing, and free-model status stay in the info popup. Combo suggestions retain raw upstream IDs, and you can still type any model. Some servers do not implement model listing; a failed refresh does not mean their chat endpoint is unusable. Save changes before testing them.

Saved API keys are never rendered back into HTML. Leaving the key field blank
when editing a connection preserves the existing encrypted value. To clear a
saved key, recreate the connection. Connections referenced by combos cannot be
deleted until those steps are removed; disable them to temporarily skip them.
Custom provider cards carry a **Custom** badge. Open a custom provider and choose
**Delete provider** to remove it and all its connections. Deletion is blocked if
any connection is referenced by a combo; preset provider tiles are not removable.

## Routing and streaming

### Model discovery limits

`GET /v1/models` includes exact numeric `context_length` and `max_output_tokens`
when present in the last refreshed model details. It also includes the
OpenRouter-style `top_provider.context_length` and
`top_provider.max_completion_tokens` for clients that recognize these extensions.
Direct IDs use the oldest enabled connection listing that model. A combo inherits
limits from **its first configured step's exact connection/model**, retaining its
`combo/<name>` ID. For example, a combo starting with `cinf/deepseek-v4.1-flash`
whose refreshed details show 1,048,576 context and 393,216 output tokens advertises:

```json
{
  "id": "combo/my-combo",
  "object": "model",
  "created": 0,
  "owned_by": "sparkrouter",
  "context_length": 1048576,
  "max_output_tokens": 393216,
  "top_provider": {
    "context_length": 1048576,
    "max_completion_tokens": 393216
  }
}
```

Refresh the connection's models to populate/update these details. Unknown or
invalid limits are omitted, never guessed from another step. Empty combos remain
listed. Metadata follows configured step order (including a disabled first
connection), not a Round robin/Random reservation or a fallback selection. These
are advisory first-model limits, **not a guarantee for every fallback target**;
routing and request/response bodies are unchanged. Clients must support these
optional discovery fields to display/use them. Scoped-key filtering still applies
only to the public IDs; an allowed combo does not expose its underlying direct ID.

### Routing behavior

- **Priority** (default) starts at the first enabled step, then follows configured order on retryable failures.
- **Round robin** rotates the starting step across enabled steps, once per request. After the last step it returns to the first. Cursors are per combo, shared across concurrent requests in one process, and reset on restart; changes to enabled steps may change the next starting step.
- **Random** chooses a uniformly random enabled starting step for each request. Repeated selections are normal; it does not guarantee equal traffic counts.
- Round robin and Random fall back cyclically through the remaining enabled steps in configured order. Each step is attempted at most once per request. Disabled providers never consume a turn. Permanent errors stop all strategies; streaming cannot switch after commitment.
- Connect/read/request failures and HTTP **401, 402, 404, 408, 429, 500, 502, 503, 504, 529** try the next step. The status policy lives in `src/routing/combo.rs`. Upstream authentication, billing, model-not-found, and Anthropic overload are properties of that target, so another connection or combo step is still tried. Anthropic JSON and pre-commit SSE error payloads use the same retry policy.
- Other HTTP failures, including **400, 403, 422**, stop the chain. No repeated attempts, cooldown state, or background work.
- Empty combos, entirely disabled combos, or exhausted chains return a sanitized OpenAI-shaped `502` error. Unknown combo names return `404`.
- Direct IDs use `<provider-slug>/<upstream-model-id>`. Only connections within that provider whose last **Refresh models** listed the upstream ID are tried, oldest first. Retryable failures never cross to another provider slug; permanent errors stop. Unknown public IDs return `404`; wholly disabled or exhausted targets return `502`. Never-refreshed connections are not direct targets. The prefix is stripped once, preserving slashes inside the upstream ID. `combo/<name>` always selects a combo; an upstream ID starting with `combo/` can be called as `<provider-slug>/combo/...`. `/v1/models` lists distinct qualified IDs from enabled connections. Combo step IDs remain arbitrary upstream strings of up to 256 characters.
- OpenAI requests remain `serde_json::Value`; only the model is replaced for each target. Unknown fields are preserved by the OpenAI adapter. Non-streaming success responses must contain a `choices` array and no non-null `error`; invalid envelopes are retryable before commitment. Valid response bytes and unknown fields remain unchanged.
- **Failover is possible only before a streaming response is committed.** The adapter obtains initial upstream data before returning the response. After that, the selected upstream owns the request. A mid-stream failure terminates the HTTP body with an error; it never switches to another provider or restarts generated text. Clients must handle incomplete streams.
- One shared pooled HTTP client uses a 10-second connect timeout, 120-second idle read timeout, and 90-second idle pool lifetime. Each attempt has 120 seconds to prepare its response; the entire pre-commit fallback phase has a 180-second ceiling. Committed streams can run longer while data continues to arrive. Redirects are rejected to protect credentials.
- Requests are limited to 4 MiB, non-stream responses to 16 MiB, and SSE events to 1 MiB. Streaming is incremental and backpressured, without buffering a complete generation. A clean upstream EOF without the protocol's completion marker is treated as an interrupted stream.

## Security and deployment

Keys are encrypted using AES-256-GCM with a fresh random nonce and authenticated provider ID. The database stores versioned ciphertext; the master key is never stored there. Secret values are wrapped with `secrecy` and key/decryption buffers use `zeroize`. Normal logs include routing IDs, model, latency, status, and fallback events; they omit credentials and upstream bodies.

Set `ROUTER_DASHBOARD_PASSWORD` to enable a single-admin dashboard login at `/login`. Sessions expire after eight hours and are revoked by logout or restart; cookies are HttpOnly and SameSite=Strict. Set `ROUTER_DASHBOARD_SECURE_COOKIE=true` behind HTTPS. There are at most 128 active sessions, and ten failed logins per minute trigger a temporary limit per socket-peer IP. Forwarded headers are not trusted; behind a reverse proxy, add a per-client login limit there because clients share the proxy's peer IP. There are no user accounts or password recovery; change the environment password and restart to change it. Login is optional for existing local installations: **without the password, the dashboard remains open**. Keep it local or behind an authenticated reverse proxy. Do not expose dashboard passwords over public HTTP.

`ROUTER_API_KEY` and dashboard-generated API keys protect gateway endpoints only; a dashboard session does not authorize API requests. Generated keys support model allowlists and expiry, are shown once, and are authenticated by a master-key fingerprint; the temporary encrypted reveal envelope is cleared on first viewing. Dashboard forms retain CSRF and origin checks; scripts/styles are local embedded assets. Provider URLs intentionally permit private/local endpoints, so dashboard access grants control over outbound connections. TLS termination belongs at your reverse proxy.

### Settings

**Settings** (`/settings`) offers browser-local Light/Dark/System appearance,
request-history retention (1–10,000 records, default 10,000), a confirmed clear
history action, configuration export/import with inline preview, and read-only authentication
and system status (version, uptime, database read connectivity, Linux resident
RAM). Refresh the page to update status. Retention persists in SQLite; lowering
it immediately and irreversibly prunes older requests and attempts and updates
Overview totals. Increasing it cannot recover deleted history. Clearing history
resets totals but in-flight requests can still record new history.

Configuration transfer is credential-free, not a complete backup, and does not
include settings. For a full backup, use SQLite online backup or stop cleanly
before copying the database; save the existing master key separately. Deployment
secrets, bind address and database location are not exposed or editable here.
Settings shares dashboard login, CSRF and origin protections. No background
monitoring worker is added. Theme controls require JavaScript; retention,
clear, transfer and status work without it.

### Request history and configuration transfer

**History** (`/history`) shows routed API and test-chat requests, outcomes, ordered attempts, connection/model names, latency, and token counts when upstream usage is available. Authentication, permission, and boundary-validation rejections are not recorded. **Basic** (default) stores metadata only. **Off** records no new routed history. **Extended**, explicitly confirmed in Settings, also stores sensitive request/response body captures. Retention defaults to the newest 10,000 requests (a lower cap can be selected in Settings), with pagination, aggregate statistics, and a CSRF-protected clear action. Streaming history distinguishes completion, interruption, and cancellation; a successful HTTP status alone is not a completed generation. Streaming token counts require upstream usage events (for OpenAI-compatible servers, request `stream_options: {"include_usage": true}` if supported). Unknown usage remains unknown, not zero. Recording is best-effort; inserts do not delay response commitment and finalization is bounded. Reading the logging preference before routing has a 200 ms deadline (failure disables recording for that request); database write failures do not fail generation. Streaming finalization waits for the insert so a fast completion is not left recorded as still streaming. An interrupted process may lose final metadata; unfinished streams are marked interrupted on restart.

Extended is a development tool, not an HTTP wire logger: it captures the validated
gateway request JSON (public model ID, after removing Test chat CSRF) and final
gateway response bytes (JSON or SSE; Anthropic translated to OpenAI). Attempts
show upstream model IDs separately. It excludes HTTP headers, configured transport
credentials, intermediate fallback bodies and raw upstream errors. **Body content
is unredacted and may itself contain secrets.** Each request/response capture is
limited to 64 KiB, with truncation and partial-capture notices. Inspect captures
via History; pages are HTML-escaped and `no-store`. Bodies are stored **unencrypted
in SQLite**, including its WAL and backups. Secure dashboard access, use test data,
and keep retention low (the maximum permits roughly 1.25 GiB of body text plus
metadata/database overhead). Mode changes apply before routing to new requests,
not requests already in flight, and do not erase existing records. Clear/prune
cascades to body captures; deletion is not a secure wipe of WAL, free pages or
backups. Off pauses new Overview totals but does not suppress operational
`tracing` logs. Configuration export never includes captures or logging settings.

**Overview** (`/`) uses that same retained history, including finished API and
test-chat requests; pending streams are excluded. Its five summary cards show
requests, input, output, cache-read, and USD spend, with a top-ten provider
leaderboard ranked by requests (distinct requests per provider). A retry is
counted as an attempt, not an additional request for the leaderboard; usage is
from the selected successful attempt only, so retries are not double-counted.
Cache reads are a subset of input, not an additional input quantity. Missing
usage/cost is unknown, not zero: visible coverage counts distinguish partial
totals from invoice-grade totals. The Anthropic adapter omits missing token
counts in JSON and streaming usage; total tokens require both input and output
counts. Explicit zero remains zero. USD spend is shown only when explicitly
reported as USD; non-USD or missing prices are not estimated and display as
unknown. Clearing history clears these statistics, not lifetime totals.

**Settings → Backup & restore** (`/settings`) exports versioned JSON containing provider groups, named connections, and combos with ordered steps/strategies. It excludes provider credentials, gateway API keys, cached model lists, and history. This is configuration transfer, **not a complete backup**. Paste JSON to preview it, then confirm import. Imports are validated again and applied in one transaction with fresh IDs; existing configuration is never overwritten, and conflicting combo names reject the entire import. Distinct and empty custom groups are preserved. **All imported connections start disabled**: review endpoints, enter required provider keys, enable the connections, and refresh models for direct-model routing. Exported URLs and names may still be sensitive; share files carefully.
Group slugs are exported too. Version 1 documents without a slug remain accepted;
imports preserve requested slugs when available and add a suffix on conflicts.
Check the imported provider's displayed slug before updating client model IDs.

One import is limited to 2 MiB of JSON, 100 explicit groups, 500 connections,
200 combos, 50 steps per combo, and 2,000 total steps (the encoded form must
also fit the 4 MiB HTTP body limit). Split larger configurations into smaller
documents with their required references. Ungrouped custom connections receive
their own new provider groups so they remain manageable in the dashboard.

The test page itself is a dashboard page, but `POST /test/chat` additionally
requires the same gateway authentication as `/v1/*`, plus dashboard CSRF and
origin checks. It never reveals saved provider credentials or raw upstream errors.

The dashboard uses `Referrer-Policy: same-origin`: native form submissions retain their Origin for validation, and external destinations receive no referrer. If a submission is rejected, reload the form after a server restart (CSRF tokens change), use one consistent hostname and port, and ensure a reverse proxy preserves the browser-facing Host header.

### Health and readiness

- `GET /healthz` returns `200 {"status":"ok"}` while the HTTP server is responsive.
- `GET /readyz` checks database access with `SELECT 1` and a two-second deadline;
  it returns `200 {"status":"ready"}` or `503 {"status":"unavailable"}`.
- Both support HEAD, use `Cache-Control: no-store`, and are public even when
  dashboard/API authentication is enabled. They never call upstream providers or
  disclose configuration/errors. Readiness checks database access, not provider
  availability, write permissions, or model health.

Use `/healthz` for liveness and `/readyz` for reverse-proxy/orchestrator readiness.
From the host: `curl --fail http://127.0.0.1:8080/readyz`. The minimal Docker image
does not contain curl; configure probes externally or supply your own probe tool.

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

Only `/v1/chat/completions` and `/v1/models` are implemented as provider APIs. Requests route via `combo/<name>` or via an upstream model ID from a connection's last refresh. Combos support Priority, Round robin, and Random starting-step selection with ordered fallback. Direct IDs try matching enabled connections from oldest to newest, once each. No embeddings, Responses API, Gemini, OAuth, routing optimization, price optimization, queues, caching, telemetry platform, or background model discovery. Model IDs are arbitrary strings. Steps can be edited inline without reordering. Model listing includes configured combos even if empty or disabled, plus distinct model IDs from enabled connections, filtered by key permissions.

Anthropic is a translation subset, not full OpenAI feature parity. It supports system/user/assistant/tool messages, text and images, function tools, and text/tool streaming. Multiple choices, developer/named messages, legacy functions, strict schemas, structured response formats, log probabilities, nonzero penalties, and other untranslatable generation options are rejected. Bookkeeping fields and image-detail hints are omitted; unknown extension fields pass through. Thinking blocks are not exposed as answer text. `max_tokens` defaults to 8192 if unspecified. Set `max_tokens` or `max_completion_tokens` when a model rejects that ceiling or a different budget is required. Provider connection testing requires a `/models` endpoint. Streams idle for 120 seconds end as incomplete responses. There is no master-key rotation workflow in this version.
