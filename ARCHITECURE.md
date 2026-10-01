# SparkRouter architecture

## Design goal

SparkRouter keeps configuration management and request routing in one Rust
application. It is not a reduced copy of every OmniRoute subsystem.

```text
Browser ──> Askama dashboard + HTMX ──> SQLite configuration
                                               │
OpenAI client ──> /v1/* ──> combo or direct model ──> protocol adapter ──> upstream
                                               │
                                      shared Reqwest client
```

There is one Cargo package with a library (`src/lib.rs`) and executable
(`src/main.rs`). The library lets integration tests exercise the actual router
without starting a separate gateway process. No frontend service, distributed
state, cache, queue, or routing worker is involved.

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `main.rs` | Logging setup, configuration, listener, graceful shutdown |
| `config.rs` | Environment parsing and startup validation |
| `app.rs` | Shared `AppState`, one HTTP client, router composition and middleware |
| `error.rs` | `thiserror` categories and sanitized HTTP error envelopes |
| `crypto.rs` | Versioned AES-256-GCM credential envelopes |
| `db/mod.rs` | SQLite pool setup and embedded SQLx migrations |
| `db/providers.rs` | Provider CRUD and ciphertext persistence |
| `db/combos.rs` | Combo CRUD, step order, transactional mutations |
| `web/` | Form validation, CSRF verification, rendering and dashboard actions |
| `api/chat.rs` | JSON boundary validation, combo-or-direct dispatch, and overall routing deadline |
| `api/models.rs` | Combos and refreshed connection models listed as OpenAI models |
| `routing/combo.rs` | Sequential combo fallback loop and retryable status helper |
| `routing/direct.rs` | Direct model lookup and the same pre-commit fallback policy |
| `providers/mod.rs` | Protocol selection, authentication, bounded HTTP bodies and stream commitment |
| `providers/openai.rs` | OpenAI passthrough and terminal SSE tracking |
| `providers/anthropic.rs` | Anthropic request, response, and event translation |
| `providers/sse.rs` | Private incremental SSE framing shared by both adapters |

`APP_NAME` in `src/lib.rs` centralizes the dashboard's display name. The package,
binary, and API `owned_by` value are separate identifiers.

## Startup and state

1. Initialize `tracing-subscriber`, respecting `RUST_LOG`.
2. Read environment configuration and validate the master key and bind address.
3. Construct one pooled Reqwest client; disable redirects.
4. Open SQLite with foreign keys, WAL, a five-second busy timeout, and embedded
   migrations. File-backed databases use at most five connections; in-memory
   test databases use one.
5. Generate a process-wide random CSRF token and compose API/dashboard routers.
6. Bind the listener and serve until shutdown. Close the database pool afterward.

`AppState` clones share the SQLx pool and Reqwest client. Crypto state, optional
gateway secret, optional dashboard authentication state, and CSRF token are shared through `Arc`. There is no global
provider cache: routing reads persisted configuration for each request. Per-combo
round-robin cursors are ephemeral shared runtime state, protected by a short
mutex reservation with no lock held during database or upstream I/O. They reset
on process restart and dashboard deletion removes the combo's cursor.

## Persistence model

| Table | Important columns and constraints |
| --- | --- |
| `providers` | UUID string ID, name, base URL, protocol, encrypted key blob, enabled flag, creation/update timestamps |
| `combos` | UUID string ID, unique name, strategy (`priority`/`round_robin`/`random`), creation/update timestamps |
| `combo_steps` | UUID string ID, combo ID, provider ID, position, arbitrary model string; unique `(combo_id, position)` |
| `custom_provider_groups` | Persistent custom provider ID, display name, base URL, protocol; may have no connections |
| `provider_group_connections` | Explicit connection/provider-group membership; connection ID remains the existing `providers.id` |
| `api_keys` | UUID string ID, display name, short prefix, master-key HMAC fingerprint, optional one-time reveal envelope, restricted flag, optional Unix expiry timestamp |
| `api_key_models` | Exact public model ID allowlist, unique `(key_id, model)`, cascading key deletion |

Combo deletion cascades to steps. Provider deletion is restricted while steps
reference it. An index supports provider-reference lookups. There is no models
table or provider-specific vendor metadata.

Combo names are case-sensitive, 1–64 ASCII letters/digits or `-`, `_`, `.`.
Model strings are trimmed, nonempty, and at most 256 characters. Stored step
positions are zero-based; the dashboard displays one-based numbers.

Step mutations use `BEGIN IMMEDIATE`, taking the SQLite write lock before
reading positions. Adds append, removals renumber, and swaps temporarily park a
step at position `-1` inside the transaction to avoid uniqueness conflicts.
Step target edits update the existing row in the same immediate transaction,
preserving its ID, combo, position, and the combo's strategy. Invalid connections
or cross-combo step IDs never mutate configuration.
Timestamps are updated on edits; provider creation time is preserved by upserts.

Routing snapshots the ordered step list, then loads each provider as its attempt
begins. Changes made during an active request are not a single transactional
configuration snapshot. A provider removed after its snapshotted step is deleted
is skipped when no longer found; other database failures remain errors.

## Public API and fallback

The public router is mounted at `/v1` separately from the dashboard:

- `POST /v1/chat/completions`
- `GET /v1/models`

If `ROUTER_API_KEY` is configured, or at least one dashboard-generated API key
exists, API middleware requires `Authorization: Bearer <key>`. The environment
key is compared with `subtle`. Generated keys are matched by a master-key HMAC
fingerprint and checked against expiry, then their authorization policy is loaded
in one read transaction. A request-local `KeyAccess` extension carries either
unrestricted access or an exact model-ID allowlist; empty selected access denies
all models and never defaults to unrestricted. The environment key remains
unrestricted. Existing generated keys migrate to unrestricted/non-expiring. If neither
kind of key is configured, these endpoints permit unauthenticated requests.
Expired keys count as configured until revoked. Authentication failures return
401, and valid keys requesting a disallowed model return OpenAI-shaped 403 before
any routing/strategy reservation/history/upstream work. `/test/chat` applies the
same policy. Model listing filters both combos and direct IDs by this policy.
Scopes are case-sensitive public IDs; allowing a combo permits its full fallback
chain but not direct access to its step models. Renaming a combo does not rewrite
key scopes; reusing an allowed public name authorizes that new configuration.
Key expiry is an absolute Unix timestamp computed from SQLite's clock and checked
at request start. Policy changes/revocation/expiry affect new requests, not active
generations. Updates serialize the flag, expiry, and allowlist in an immediate
transaction without changing key fingerprints or reveal envelopes.
Model listing returns configured combos as
`combo/<name>`, including empty combos and those with no enabled provider.
It also returns each distinct model ID from the last refresh of an enabled
connection. IDs that start with `combo/` are not listed as direct models.

For chat requests:

1. Parse the body into `serde_json::Value`, requiring an object, a nonempty model
   string, and a boolean `stream` if supplied. `combo/<name>` selects a combo.
   Any other ID is a direct model.
2. Combo requests look up the combo by name and load steps in ascending position
   order. Direct requests load connections whose stored model list contains that
   exact ID, oldest connection first. A missing list is not a direct target.
3. Priority uses configured order. Round robin and Random snapshot enabled steps
   before selecting a starting step; disabled or concurrently removed providers
   are skipped and do not consume turns. Providers are rechecked before attempts.
4. Clone the request for the attempt and replace `model` with the step's actual
   upstream model. A direct request already carries that ID, so the replacement
   is the same string. Let the selected protocol adapter prepare and send it.
5. Return the first successful response. Retryable failures advance to the next
   combo step or matching connection; permanent errors stop the chain immediately.
   Round robin reserves one
   starting turn per request, shared by all handlers; retries do not advance it
   again. Random uses OS randomness with rejection sampling to avoid modulo bias.
   Both visit remaining steps cyclically from the selected start, at most once.

The explicit retryable HTTP statuses are **408, 429, 500, 502, 503, 504**.
Transport failures, timeouts, and invalid/incomplete upstream success payloads
can also fail over before commitment. Local credential/configuration failures
are permanent. Most malformed-request and authorization statuses stop the chain.
There are no repeated attempts, hedged requests, cooldowns, or adaptive routing.

Unknown combos and model IDs that no connection lists return `404`. Exhausted,
empty, or wholly disabled chains, including a direct model listed only by
disabled connections, return `502`. Errors have an OpenAI-shaped `error` object
without raw upstream bodies.
Upstream authentication errors are labeled `upstream_error`, distinct from local
gateway `authentication_error`. Rejected redirects are surfaced as gateway errors,
not forwarded to the client as redirect instructions.

## Protocols rather than vendors

`Protocol::{OpenAi, Anthropic}` is the dispatch boundary. The dashboard catalog
is static public metadata for OmniRoute API-key chat presets whose endpoint and
auth header match one of these adapters. OpenRouter, Groq, DeepSeek, and similar
vendors share OpenAI handling through configuration. No trait hierarchy or
per-vendor implementation is necessary.

Base URLs retain their configured API prefix and lose trailing slashes. Only
HTTP(S) URLs without embedded credentials, query strings, or fragments are
accepted. HTTP/private endpoints are intentionally allowed for self-hosted models.

| Protocol | Generation endpoint | Authentication | Response handling |
| --- | --- | --- | --- |
| OpenAI | `{base_url}/chat/completions` | Bearer key when configured | Preserve request extensions and original valid JSON response bytes; raw SSE passthrough |
| Anthropic | `{base_url}/messages` | `x-api-key` and `anthropic-version: 2023-06-01` | Translate messages, tools, text/image input, usage, finish reasons, and SSE to OpenAI shapes |

Both protocols test connectivity with `GET {base_url}/models`. That is not a
generation test or a guarantee of access to a particular model.

Anthropic translation deliberately rejects unsupported semantics rather than
pretending to provide full OpenAI parity. Its default token budget is 1024.
Unknown extension fields can pass through, while known bookkeeping fields are
omitted. Thinking blocks are not exposed as answer text. See the README for the
current supported subset.

## Streaming ownership and limits

**The fallback loop ends when the adapter returns a response to Axum.**

```text
Before response commitment:
  upstream failure / timeout / empty stream -> retry next step if eligible

After response commitment:
  upstream interruption -> body error and terminated stream
  never re-enter fallback, restart text, or emit a fake successful completion
```

The shared streaming helper prefetches the first nonempty output before returning
the response. For OpenAI, this is raw upstream data; even a comment can commit
the stream. For Anthropic, it is the first translated chunk, usually the
assistant-role chunk from `message_start`. Commitment does not wait for the first
answer token or for an acknowledgment that a client received it.

SSE parsing handles fragmented UTF-8, LF/CRLF lines, and multiple events per HTTP
chunk. OpenAI bytes are not re-encoded; the framing tracker watches for `[DONE]`.
Anthropic events are translated incrementally until `message_stop`, then `[DONE]`
is emitted. EOF without a completion marker is an error. The upstream body is
dropped on completion, cancellation, or error. Streams remain backpressured.

| Setting | Current limit |
| --- | --- |
| Incoming request body | 4 MiB |
| Non-streaming upstream response | 16 MiB |
| Individual SSE event | 1 MiB |
| Connect timeout | 10 seconds |
| Upstream idle read timeout | 120 seconds |
| Each attempt's response-preparation deadline | 120 seconds |
| Whole pre-commit fallback deadline | 180 seconds |
| Provider connection test and model refresh | 15 seconds |
| HTTP pool idle lifetime / idle connections per host | 90 seconds / 4 |

Committed streams are not subject to the pre-commit deadlines; they can run
longer while data arrives. The individual read timeout still applies.

## Dashboard and trust boundaries

`GET /test` renders a combo chat playground with embedded JavaScript.
`POST /test/chat` requires dashboard CSRF/origin checks and the same gateway
authentication as the public API. It forces non-streaming responses and invokes
the existing combo fallback loop with an optional request-local attempt trace.
Strategy selection, deadlines, and shared round-robin cursors are unchanged.
The response wraps the completion or sanitized error with ordered connection
names/IDs, models, configured step numbers, outcomes, and attempt latencies;
no endpoint URLs, credentials, or upstream error bodies are added to the trace.
Public API response formats remain unchanged. Conversation history lives in the
page, not SQLite or browser storage; generated text is rendered as text, not HTML.

Askama renders `/providers`, `/providers/new`, `/providers/{id}` and the matching
combo pages. POST actions handle create/edit/delete, connection tests, model-list refresh, and step
add/edit/remove/up/down operations. HTMX swaps small fragments; ordinary mutation
forms generally use 303 redirects back to GET pages. There is no SPA state store.
`POST /combos/{id}/duplicate` requires dashboard CSRF/origin checks and copies
the combo and its ordered steps in one immediate transaction. A unique copy
name and fresh combo/step IDs are allocated; provider connection IDs are reused.
The list's clipboard control uses embedded JavaScript to copy `combo/<name>`;
it is hidden when the browser's clipboard API is unavailable.

The providers index is a preset/custom tile catalog. `/providers/catalog/{slug}`
and `/providers/groups/{id}` show named connections and an add-connection popup.
The popup uses native HTML popover controls, retaining ordinary POST forms and
CSRF fields without inline scripts or a JavaScript dependency. Browsers without
popover support display the form inline.
Custom providers are stored separately from their connections. `POST
/providers/custom` creates a named, empty provider from its name, base URL, and
protocol; its card remains after the last connection is deleted. Membership is
explicit, so custom providers sharing an endpoint remain separate. Each
connection retains its provider UUID, encrypted credential, enabled flag, and
combo references. The combo add-step form chooses a provider, then one of
its connections, and targets that connection explicitly. Preset add forms take their URL and protocol from
the server-side catalog; custom connection forms take them from the stored
provider. Legacy custom connections are assigned to groups by a new migration
without changing ciphertext. The legacy `POST /providers` connection form is
retained for compatibility.
Each step has a native details/edit form with the same provider/connection/model
controls. Picker JavaScript is form-scoped and reinitializes swapped fragments;
unique per-step labels and datalists prevent controls from interfering. Without
JavaScript, the submitted connection still selects the target directly.
`POST /providers/groups/{id}/delete` accepts canonical custom-provider IDs only
and requires dashboard CSRF/origin checks. It deletes the provider and all member
connections in one immediate transaction; combo foreign keys block the entire
operation while any connection is referenced. Preset tiles are not deletable.

Every mutation form supplies the process's CSRF token. Unsafe dashboard requests
also reject cross-site fetch metadata and mismatched supplied Origin/Host values.
`Referrer-Policy: same-origin` preserves the browser's Origin on native form
POSTs while withholding referrers from external destinations. `no-referrer`
would turn those form origins into `null`, which the origin check rejects.
These protections are independent of the optional dashboard login. **Dashboard access permits
changing credentials and making outbound connections, including to local hosts.**
`ROUTER_DASHBOARD_PASSWORD` enables dashboard-specific middleware and a CSRF/origin-
protected login form. The password remains in process environment/state, never
SQLite. Domain-separated master-key HMAC fingerprints are compared in constant
time. Random 256-bit session cookies are HttpOnly, SameSite=Strict, optionally
Secure; a bounded process-local session map enforces eight-hour expiry and logout
revocation. Restart invalidates all sessions. Bounded per-socket-peer accounting
permits ten failed logins per minute; forwarding headers are deliberately ignored.
Reverse proxies should apply their own per-client limits. Without
the variable, the dashboard remains open. Gateway API authentication does not
cover the dashboard, and dashboard sessions never authorize `/v1/*` or test chat.
TLS is terminated outside the application.

Askama escapes user-controlled HTML. Embedded scripts/styles are served from
`/static/`; CSP permits self-hosted assets and blocks framing. HTMX evaluation,
script injection, and inline indicator styles are disabled. Small local JavaScript
reports HTMX failures using text, not injected server HTML.
Dashboard navigation and action icons use a locally embedded Lucide SVG sprite;
decorative icons are hidden from assistive technology and retain text labels.
The icon license is embedded alongside the sprite. No external icon service or
font is loaded.
The embedded theme script applies a browser-local Light/Dark preference before
the stylesheet loads. Without a saved choice, CSS follows `prefers-color-scheme`;
the button displays the resolved mode and toggles directly to its opposite.
Theme selection does not mutate server configuration or relax CSP.

## Credential protection and observability

### Public local probes

`health.rs` exposes `/healthz` and `/readyz` on a router merged outside dashboard
and gateway authentication. Liveness performs no database work. Readiness executes
`SELECT 1` against the shared pool with a two-second acquisition/query deadline.
Both return fixed JSON statuses, no-store headers, and no provider requests or
configuration/errors. Failed readiness is 503; liveness remains 200 while the
server responds. These are not upstream availability or SQLite writability checks.

Configured API keys use AES-256-GCM with a random 96-bit nonce and the provider
ID as authenticated associated data. The blob is:

```text
version byte (1) | nonce (12 bytes) | ciphertext | GCM authentication tag
```

The master key is 32 bytes decoded from `ROUTER_MASTER_KEY`, held in a zeroizing
buffer and never written to SQLite. Secret strings use `secrecy`; decrypted
temporary buffers use `zeroize`. An empty blob is the deliberate sentinel for a
provider without authentication. A malformed nonempty blob fails closed.

The dashboard never decrypts keys for display. Blank edits keep existing bytes
unchanged. Key rotation/recovery tooling is not implemented; keep the original
master key securely available across restarts and backups.

Request middleware generates/propagates `x-request-id` and creates tracing spans.
Routing records combo, provider ID, model, status, latency, and fallback events.
HTTP errors are logged through sanitized categories rather than full Reqwest
errors. Do not enable or introduce credential/header/body dumps.

## Extension rules

### Local request history and configuration transfer

`history.rs` observes routing traces and response bodies without changing upstream
bytes, response envelopes, strategy state, or fallback semantics. JSON responses
carry best-effort usage metadata in internal response extensions. The streaming
observer frames bounded SSE events and reads usage without re-encoding events;
malformed observer data never alters forwarding. Terminal body errors record
interruption; dropped bodies record cancellation (or success if `[DONE]` was
already observed). Drop finalization is a bounded asynchronous database write,
not a recurring worker. Recording failures are logged only as fixed categories
and never fail generation. Startup marks orphaned streaming rows interrupted.
The `request_history` and `request_attempts` tables contain metadata snapshots
only, with no foreign keys to live configuration that could block deletion.
Inserts prune to 10,000 requests; attempts cascade with history deletion. The
dashboard lists 50 rows per page with aggregates and a protected clear action.

Configuration transfer uses a strict versioned JSON schema and consistent export
snapshot, excluding all secret material, API keys, cached models, and history.
Preview performs validation; confirmation repeats validation under a write
transaction and inserts fresh IDs without modifying existing rows. Invalid
references or conflicting combo names roll back the entire import. Imported
connections have empty credential blobs and start disabled, regardless of their
exported enabled flag. Endpoints must be reviewed and credentials set before
enabling. Provider groups, including distinct groups sharing an endpoint and empty
groups, and combo strategy/step ordering survive transfer. This is not a database
backup or master-key rotation mechanism.

- Add protocols in `src/providers/` with explicit request/JSON/SSE translation,
  preserving the existing retry and commitment contracts.
- Add new database changes as SQLx migrations, retaining ordering constraints.
- Add dashboard authentication on the dashboard router, independently of the
  gateway's bearer middleware.
- Prefer configuration over vendor-specific code; avoid adding new services or
  generic abstractions without an actual requirement.

See [AGENTS.md](AGENTS.md) for contributor invariants and [TESTING.md](TESTING.md)
for the regression checks that protect these boundaries.
