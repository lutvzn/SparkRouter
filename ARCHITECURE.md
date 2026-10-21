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
state, response cache, or routing worker is involved; the only in-process queue
holds request history, and the only configuration cache is described below. Optional combo
placement hints are bounded process-local state, not saved prompt/completion data.

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `main.rs` | Logging setup, configuration, listener, graceful shutdown |
| `config.rs` | Environment parsing and startup validation |
| `app.rs` | Shared `AppState`, one HTTP client, router composition and middleware |
| `error.rs` | `thiserror` categories and sanitized HTTP error envelopes |
| `crypto.rs` | Versioned AES-256-GCM credential envelopes |
| `db/mod.rs` | SQLite pool setup and embedded SQLx migrations |
| `db/config_cache.rs` | Process-local copies of request-path configuration, invalidated by every main-pool commit |
| `db/providers.rs` | Provider CRUD and ciphertext persistence |
| `db/provider_favorites.rs` | Dashboard-only tile favorites; migration 0020 seeds defaults once, custom preferences cascade on group deletion |
| `db/combos.rs` | Typed combo CRUD, step order, transactional mutations |
| `db/model_categories.rs` | Explicit connection/model classification, protocol/modality validation |
| `web/` | Form validation, CSRF verification, rendering and dashboard actions |
| `api/chat.rs` | JSON boundary validation, authorization/admission, and response history/permit wrapping |
| `api/media.rs` | Shared media validation/admission and bounded replayable multipart inputs |
| `providers/media.rs` | Compatible media, fal inference/queue polling, MiniMax/xAI native generation, Deepgram/ElevenLabs speech, binary commitment |
| `api/models.rs` | Scoped OpenAI model discovery with optional cached token limits; combos inherit first configured step details |
| `routing/mod.rs` | Combo-or-direct dispatch, shared attempt execution/traces, and pre-commit deadlines |
| `routing/combo.rs` | Sequential combo fallback loop and retryable status helper |
| `routing/affinity.rs` | Per-caller/combo session, prompt, and timed placement hints; successful-body recording |
| `routing/direct.rs` | Direct model lookup and the same pre-commit fallback policy |
| `providers/mod.rs` | Protocol selection, authentication, bounded HTTP bodies and stream commitment |
| `providers/openai.rs` | OpenAI passthrough and terminal SSE tracking |
| `providers/anthropic.rs` | Anthropic request, response, and event translation |
| `providers/systemone.rs` | Native evaluation passthrough and SystemOne model-list parsing |
| `providers/sse.rs` | Private incremental SSE framing shared by both adapters |

`APP_NAME` in `src/lib.rs` centralizes the dashboard's display name. The package,
binary, and API `owned_by` value are separate identifiers.

## Combo category policy

Combos persist one category: text, decision, image, video, music, tts, stt, or
embedding. RequestContext maps the selected endpoint to its category (image edits
share image; translation shares stt; Responses shares text). It also limits
eligible protocols per endpoint: Responses accepts OpenAI and xAI connections,
embeddings only OpenAI-protocol connections. API/dashboard handlers reject category mismatch
before key admission; routing repeats the check and revalidates all step
classifications before affinity/rotation or any upstream attempt. A metadata or
protocol change cannot silently turn an existing chain into a mixed-category
fallback. Provider tags remain independent display labels.

Step add/edit declares unknown connection/model IDs in the selected category in
the same immediate transaction as the mutation. Explicit cached output and speech
input modalities constrain declarations; IDs are never classified by name and
missing lists do not block manual entries. Existing declarations cannot change
while referenced; removing every reference permits explicit reclassification.
Changing a combo category requires an empty chain, atomically with its other
settings. Duplication/import validates classifications and preserves category;
transfer rejects cross-category declarations even before its transaction.

Migration 0019 adds categories and declaration storage without deleting existing
configuration. SystemOne-only legacy chains become decision; other chains default
to text, with incompatible legacy configurations failing closed. The current
installation's one-time, user-approved combo deletion is not embedded in migrations.
The Test picker filters options by mode; the Combos list filters via ordinary GET
forms. `providers/model_catalog.rs` holds a small, exact provider-scoped mapping
of documented IDs. OpenAI requires its actual host; native media protocols scope
their own entries. `build.rs` additionally projects models.dev metadata into an
embedded exact-ID index, scoped by matching preset endpoint and protocol. It
uses a bounded, timeout-limited, redirect-disabled public download with a bundled
snapshot fallback; there are no runtime external lookups. Only conservative
category evidence is retained (image/video outputs, audio-only input to text,
or text-input/text-output tool-capable chat). Ambiguous audio and text-only
non-tool models remain unknown. Hand-reviewed entries take precedence. No ID
prefixes, substrings or automatic model-list additions are used, and OpenAI
Sora is excluded. `Category::matches_model`
is shared by picker, transactional mutations/import, and pre-attempt routing.
Explicit endpoint metadata overrides advisory catalog entries, while contradictory
modalities always fail closed. Persisted declarations cannot bypass this policy;
newly recognized incompatible legacy declarations require manual correction.
Direct routing remains unchanged and does not enforce combo classifications.

Step picker suggestions require positive evidence (a matching explicit
declaration, catalog entry, suitable modalities/endpoints, or a single-category protocol); missing
metadata is not treated as a match for every category. Add/edit and HTMX
fragments share the same filtering. Discovery exposes an additive category field
for combos only.

## Dashboard diagnostics

`model_categories::evidence` uses existing compatibility/positive-evidence rules
to expose classification provenance without broad name heuristics. Upstream
metadata precedes advisory catalogs, and conflicting declarations are flagged for
review. The declaration editor uses the same immediate-transaction validation
as combo step mutations; referenced models cannot change category. Classification
editing/reset lives in each model's Info popup. Reset deletes a declaration under
an immediate write lock only if automatic evidence preserves every referencing
combo category. Routing permits a missing declaration only with positive automatic
evidence matching the requested category; unknown IDs still require declarations.
Build metadata
records live-download time or the fixed bundled snapshot date.

Migration 0021 adds connection refresh status and metadata-only routing snapshots.
Successful list replacement clears refresh errors in the same transaction; failed
refreshes update only a fixed sanitized error/attempt timestamp, leaving the last
successful model list intact. Staleness is computed against SQLite's clock at
24 hours, advisory only. No worker or automatic polling is added.

Combo attempt traces carry selection snapshots (strategy start, actual start,
affinity mode/result) and sanitized failure reasons. Retryable HTTP errors retain
only status codes. `history` saves these with the normal record transaction,
without extra pre-response I/O. `request_routing` cascades on history deletion;
Logging Off bypasses it. Selected attempts never imply successful stream EOF.
Historical rows without snapshots show unavailable diagnostics rather than using
current combo settings. No session identifiers, prompts, endpoint URLs or raw
upstream error bodies are retained.

## Startup and state

1. Initialize `tracing-subscriber`, respecting `RUST_LOG`.
2. Read environment configuration and validate the master key and bind address.
   After migrations, verify the key against `master_key_material` and load the
   fingerprint key; load the stored dashboard password hash, or generate and log
   a first-run setup code when there is none.
3. Construct one pooled Reqwest client; disable redirects.
4. Open SQLite with foreign keys, WAL, a five-second busy timeout, and embedded
   migrations. File-backed databases use at most five connections; in-memory
   test databases use one that is never retired for idleness or age, since
   closing it would discard the database. A second, single-connection pool
   is owned by the request-history batch writer (`db/history_writer.rs`); it
   commits with `synchronous = NORMAL`. In-memory databases reuse the main pool
   because each connection is a separate database.
5. Generate a process-wide random CSRF token and compose API/dashboard routers.
6. Bind the listener and serve until shutdown. Afterward, wait up to 10 s for
   queued request history to commit, then close the database pool.

`AppState` clones share the SQLx pool and Reqwest client. Crypto state, optional
gateway secret, optional dashboard authentication state, and CSRF token are shared through `Arc`. Request-path
configuration (logging mode, generated keys, combo routing snapshots, connections,
and direct listings) is read through `db/config_cache.rs`, explicitly requested to
take SQLite off the request path. A commit hook on every main-pool connection
invalidates all of it: a commit marks the cache when it starts and settles once its
connection returns to the pool or closes, and lookups that overlapped any unsettled
commit are not kept, so a request never sees configuration older than the last
finished commit. Only found entries are kept, bounding it by the stored
configuration. History batches use their own pool and do not invalidate it. Writes
from another process (for example the `sqlite3` shell) are not noticed until this
process commits, so edit a running gateway through the dashboard. Per-combo
round-robin cursors are ephemeral shared runtime state, protected by a short
mutex reservation with no lock held during database or upstream I/O. They reset
on process restart and dashboard deletion removes the combo's cursor.

A second short mutex protects at most 4096 affinity entries. The key includes
combo ID, routing revision, authenticated caller namespace, and a domain-separated
master-key HMAC of the explicit session ID or Smart prompt. Sticky uses a fixed
identity within that caller/combo namespace. Only routing target IDs, digests,
and monotonic expiry/last-success instants are retained. No locks span I/O, and
expiry/oldest-entry eviction runs inline; there is no worker or history dependency.
Every mode expires the combo's 1–1440 minute affinity timeout after last success
(stored as `sticky_minutes`; migration 0027 set existing Basic/Smart combos to
their former fixed 30 minutes). Generated keys have distinct namespaces; environment-key and
open access each share one namespace. Concurrent misses are not serialized
through upstream completion.

Affinity overrides the reserved strategy start, not its accounting or retry
policy. Round robin still reserves exactly once per request. Enabled steps are
snapshotted before preference lookup, target step/connection/model are matched,
and fallback follows configured cyclic order from a hit. Provider enablement is
rechecked before calls. Retryable failures clear that request's hint before
fallback; a successful backup becomes the new target. Non-streaming adapters
already validate/buffer success; streaming bodies are wrapped without changing
bytes/errors and only successful EOF establishes/renews affinity. Body error or
drop does not establish a new hint or re-enter fallback.

`api/chat.rs` and `web/test.rs` pass authenticated caller context and one selected
session header into routing; no bearer header is retained or forwarded. Basic
uses explicit header/body IDs; Smart additionally fingerprints consecutive
initial system/developer messages, the first user message, and tools using exact
JSON content (ordered object maps). Serialized fingerprint material is temporary
and zeroizing. IDs are bounded/validated with sanitized errors, never logged.
Off/Sticky ignore session fields for routing. Header precedence and accepted
body fields are documented in the README. Smart requires a first user message
and is a conservative placement hint, not conversation identity or evidence of
an upstream cache hit.

## Persistence model

| Table | Important columns and constraints |
| --- | --- |
| `providers` | UUID string ID, name, base URL, protocol, encrypted key blob, enabled flag, creation/update timestamps |
| `combos` | UUID string ID, unique name, strategy (`priority`/`round_robin`/`random`), affinity mode (`off`/`basic`/`smart`/`sticky`), Sticky minutes (1–1440), routing revision, creation/update timestamps |
| `combo_steps` | UUID string ID, combo ID, provider ID, position, arbitrary model string; unique `(combo_id, position)` |
| `custom_provider_groups` | Persistent custom provider ID, display name, base URL, protocol; may have no connections |
| `provider_group_connections` | Explicit connection/provider-group membership; connection ID remains the existing `providers.id` |
| `provider_tags` | Dashboard-only tag overrides keyed by preset/group page identity; custom group foreign key cascades deletion; JSON list of validated fixed tags |
| `provider_slugs` | Custom group ID with cascading deletion; unique saved public slug (1–64 lowercase letters/digits/hyphens) |
| `api_keys` | UUID string ID, display name, short prefix, master-key HMAC fingerprint, optional one-time reveal envelope, restricted flag, optional Unix expiry timestamp, optional requests/minute and concurrency limits |
| `api_key_models` | Exact public model ID allowlist, unique `(key_id, model)`, cascading key deletion |
| `request_history` | Bounded metadata snapshots for routed API/test-chat requests, including usage/cost and retained provider identity |
| `request_attempts` | Ordered attempt snapshots with provider slug/group name; cascade with history deletion |
| `dashboard_settings` | Singleton retention (1–10,000), logging mode (off/basic/extended, default basic), and history write buffer multiple (1/2/4/8, default 1) |
| `request_payloads` | Opt-in bounded sensitive gateway request body and truncation flag; cascade with history |
| `request_payload_responses` | The matching response body, truncation/completion flags, in its own row so the later capture never rewrites the request body; cascade with the request capture |
| `dashboard_admin` | Singleton Argon2id PHC string of the stored dashboard password and its update time; absent until first-run setup |
| `master_key_material` | Envelopes sealed with the master key: `check` (startup key verification) and, after a rotation, `fingerprint` (HMAC key for fingerprints) |

Combo deletion cascades to steps. Provider deletion is restricted while steps
reference it. An index supports provider-reference lookups. There is no models
table or provider-specific vendor metadata.

Combo names are case-sensitive, 1–64 ASCII letters/digits or `-`, `_`, `.`.
Upstream model strings are trimmed, nonempty, and at most 256 characters. Public
direct IDs allow 321 characters (64-character slug, slash, 256-character model).
New migrations preserve existing scope/history rows while extending public-ID
limits; startup assigns legacy custom groups unique slugs in one immediate
transaction. Slug generation reserves built-in aliases and the combo namespace.
Stored step
positions are zero-based; the dashboard displays one-based numbers.

Step mutations use `BEGIN IMMEDIATE`, taking the SQLite write lock before
reading positions. Adds append, removals renumber, and swaps temporarily park a
step at position `-1` inside the transaction to avoid uniqueness conflicts.
Step target edits update the existing row in the same immediate transaction,
preserving its ID, combo, position, and the combo's strategy. Invalid connections
or cross-combo step IDs never mutate configuration.
Timestamps are updated on edits; provider creation time is preserved by upserts.
An additive migration defaults existing combos to affinity Off and a 5-minute
affinity timeout. Name/strategy/affinity and step mutations increment the routing
revision, so an in-flight old snapshot cannot seed hints for an edited combo.
Dashboard combo edits/deletion also clear hints; older revisions expire naturally.
Duplication copies settings but uses fresh combo/step IDs and no runtime hints.

Combo routing loads settings and joined ordered steps/provider eligibility/model
metadata/declarations in two SELECTs within one short read transaction, regardless
of chain length. The same pure capability checks used by mutations validate every
step, including disabled steps, before strategy reservation. The transaction ends
before upstream I/O; no provider credentials are loaded into this setup snapshot.
The API boundary still checks the combo category separately before quota admission.
Each actual attempt looks up its provider configuration and namespace again (through
the configuration cache), so changes committed during an active request are not
frozen for the entire request. A provider removed after its snapshotted step is deleted
is skipped when no longer found; other database failures remain errors.

## Public API and fallback

Model discovery derives exact context/output counts from cached `Context`/`Output`
fields, never rounded UI tags. Optional numeric `context_length` and
`max_output_tokens`, plus OpenRouter-style `top_provider` equivalents, leave
existing ID/object/owner fields intact. Batched database joins bind combo metadata
to the first configured step's exact connection/model; absent details do not
borrow another step's limits. Direct metadata uses the oldest enabled listing
connection. Disabled first steps and Round robin/Random strategies do not change
this advisory configured-first-step view. Listing never calls upstreams or advances
strategy cursors, and retains exact public-ID scope filtering. No limits are
injected into generation requests or forwarded response bytes.

The public router is mounted at `/v1` separately from the dashboard:

- `POST /v1/chat/completions`
- `POST /v1/responses` (OpenAI Responses passthrough, JSON or SSE)
- `POST /v1/embeddings` (non-streaming)
- `POST /v1/systemone` (native, non-streaming evaluations)
- `GET /v1/models`
- `POST /v1/images/generations`, `/v1/images/edits`
- `POST /v1/videos/generations`, `/v1/music/generations`
- `POST /v1/audio/speech`, `/v1/audio/transcriptions`, `/v1/audio/translations`

If `ROUTER_API_KEY` is configured, or at least one dashboard-generated API key
exists, API middleware requires `Authorization: Bearer <key>` (scheme matched
case-insensitively, token exactly). The environment
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
against the system clock at request start, including for cached keys. Policy changes/revocation/expiry affect new requests, not active
generations. Updates serialize the flag, expiry, and allowlist in an immediate
transaction without changing key fingerprints or reveal envelopes.
Model listing returns configured combos as
`combo/<name>`, including empty combos and those with no enabled provider.
It also returns distinct `<provider-slug>/<upstream-model-id>` IDs from enabled
connections' last refresh. Preset namespaces use OmniRoute's default aliases;
custom namespaces are persisted independently of credentials or connection names.
Model listings, dashboard Copy buttons, and key scopes use these same public IDs.
Old raw direct scopes are not rewritten; operators must update them explicitly.

Generated-key authentication also snapshots the key ID and optional traffic
limits in the same read transaction. After boundary validation and model-scope
checks, API and test chat reserve admission in one short process-local mutex
operation. Fixed 60-second windows count gateway requests, not fallback attempts;
rejected admissions do not consume quota or start routing/history. `/v1/models`,
the environment key, and unauthenticated access are exempt. All generated-key
admissions track active requests even when unlimited, so enabling a concurrency
limit accounts for already-active requests. A drop guard releases reservations
on handler cancellation/error and owns successful streaming bodies until EOF,
body error, or drop. There is no queue, post-commit fallback, or background work.

The dashboard displays active counts and current-window rejections. Idle entries
are pruned during admission after their window expires; active entries are never
pruned. Configuration is stored in SQLite through an additive migration, while
accounting resets on restart and is not shared across processes. Updates preserve
counters and never cancel existing streams. Local limit failures are sanitized
429 responses with Retry-After, distinct from upstream 429 fallback decisions.

For chat requests:

1. Parse the body into `serde_json::Value`, requiring an object, a nonempty model
   string, and a boolean `stream` if supplied. `combo/<name>` selects a combo.
   Any other ID is a direct model.
2. Combo requests load the combo and its ordered, validated step configuration
    in a batched read transaction. Direct requests split off the first provider slug and load only its
    connections whose list contains the remaining upstream ID, oldest first.
    Custom membership overrides endpoint/preset matching. A missing list is not
    a direct target. Unassigned non-preset rows get reserved connection namespaces.
3. Priority uses configured order. Round robin and Random snapshot enabled steps
   before selecting a starting step; disabled or concurrently removed providers
   are skipped and do not consume turns. Providers are rechecked before attempts.
4. Clone the request for the attempt and replace `model` with the step's actual
    upstream model. Direct requests replace the public ID with its complete
    upstream suffix, preserving additional slashes. Let the adapter send it.
5. Return the first successful response. Retryable failures advance to the next
    combo step or matching connection within the same provider slug; permanent
    errors stop the chain immediately. No direct fallback crosses provider slugs.
   Round robin reserves one
   starting turn per request, shared by all handlers; retries do not advance it
   again. Random uses OS randomness with rejection sampling to avoid modulo bias.
   Both visit remaining steps cyclically from the selected start, at most once.

The explicit retryable HTTP statuses are **401, 402, 404, 408, 429, 500, 502, 503, 504, 529**.
Transport failures, timeouts, and invalid/incomplete upstream success payloads
can also fail over before commitment. Local credential/configuration failures
are permanent. Malformed-request statuses, including 400 and 422, stop the chain.
403 stays permanent because it can reject the request itself. Upstream 401, 402,
404, and 529 describe that target and try the next connection or combo step.
On `/v1/responses` and `/v1/embeddings` only, 405 and 501 are retryable too:
OpenAI-compatible servers often implement just chat completions, so those
statuses mean the target lacks the endpoint rather than that the request is bad.
There are no repeated attempts, hedged requests, cooldowns, or adaptive routing.

Unknown combos and model IDs that no connection lists return `404`. Exhausted,
empty, or wholly disabled chains, including a direct model listed only by
disabled connections, return `502`. Errors have an OpenAI-shaped `error` object
without raw upstream bodies.
Upstream authentication errors are labeled `upstream_error`, distinct from local
gateway `authentication_error`. Rejected redirects are surfaced as gateway errors,
not forwarded to the client as redirect instructions.

## Protocols rather than vendors

`Protocol::{OpenAi, Anthropic, SystemOne, Fal, MiniMax, Xai, Deepgram, ElevenLabs}`
is the dispatch boundary. The dashboard catalog
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

| SystemOne | `{base_url}/systemone` | Bearer key when configured | Preserve evaluation fields and native valid JSON response bytes; no streaming |

Protocols normally test connectivity with `GET {base_url}/models`. That is not a
generation test or a guarantee of access to a particular model. fal.ai has no
such probe: refresh provides labeled documented starter models, and a connection
test reports that generation must be tested. Deepgram/ElevenLabs use native model
list shapes; MiniMax/ElevenLabs catalogs add documented media IDs after an
authenticated successful refresh.

Anthropic translation deliberately rejects unsupported semantics rather than
pretending to provide full OpenAI parity. Its default token budget is 8192.
Unknown extension fields can pass through, while known bookkeeping fields are
omitted. Thinking, server-tool, and other auxiliary blocks (and citation
deltas) are skipped rather than exposed as answer text or treated as failures;
blocks without a type remain invalid. Temperatures above 1 are clamped to
Anthropic's ceiling so a mixed combo is not stopped by a permanent 400. See the README for the
current supported subset.

SystemOne model listing reads the native `models` array with `name` IDs.
The shared API handler applies authentication, model scopes, admission, capture,
and history to evaluations. The request context carries endpoint compatibility:
SystemOne requires Decision combos and chat/Test-chat requires Text combos;
mismatched categories and incompatible configured steps are rejected before
strategy selection. Per-attempt compatibility guards remain in place. SystemOne affinity namespaces
are separate from chat. The gateway checks top-level state/questions and rejects
streaming; question validation remains upstream. The adapter bounds and validates
the native success envelope (`model`, `answers`, no non-null `error`) before
commitment; invalid success bodies are retryable. Unknown fields and successful
response bytes are preserved. An additive migration widens the custom-provider
protocol constraint while retaining group IDs, membership, slugs, and credentials.

## Media routing

Media shares combo/direct routing, exact model authorization, traffic admission,
traces, and metadata history. Request context isolates affinity by endpoint and
filters protocol-incompatible steps before strategy selection and before calls.
Compatible services use JSON passthrough or bounded replayable multipart buffers;
files are shared `Bytes`, not copied/base64-encoded on each attempt. Existing
MiniMax/xAI OpenAI preset connections keep their identity; documented hosts select
the native media dialect, while custom mirrors explicitly select the protocol.
Saving an ungrouped preset connection rejects a slug already owned by a custom
group, rather than merging provider namespaces and silently broadening scopes.

Binary output is prefetched before commitment, then streamed with backpressure,
a size bound, and sanitized body failures. Its completion marker is clean EOF,
not SSE `[DONE]`; admission/affinity/history own its complete body lifetime.
Media is metadata-only even with Extended enabled. No remote media URLs are
fetched, keys are never sent to a CDN, and no raw errors are returned/logged.

Native xAI/MiniMax video and fal queue tasks submit once and poll inline. A rejected submission
can use fallback; accepted/ambiguously accepted tasks never submit a backup job.
Polling errors, malformed accepted results, timeout, or cancellation stop the
request. Dropping polling does not promise upstream task cancellation. fal poll
URLs are restricted to the configured origin/base path and submitted request ID
before credentials are attached; redirects remain disabled. Custom fal direct
endpoints are supported too. No SDK, gateway queue, worker, or persistent job
store was introduced. See [MEDIA.md](MEDIA.md) for supported adapters,
provider-specific restrictions, deferred integrations, and upstream references.

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
| Incoming JSON/dashboard body | 4 MiB |
| Multipart media body (including framing) | 32 MiB; 64 fields; 64 KiB/text field |
| Binary media output | 64 MiB |
| Non-streaming upstream response | 16 MiB |
| Individual SSE event | 1 MiB |
| Connect timeout | 10 seconds |
| Upstream idle read timeout | 120 seconds |
| Each attempt's response-preparation deadline | 120 seconds chat/SystemOne; 600 seconds media |
| Whole pre-commit fallback deadline | Per-attempt deadline × eligible targets + 60 s chat/SystemOne (+10 s media); capped at 10 minutes chat/SystemOne, 30 minutes media |
| Native video/fal queue polling | Every 2 seconds, up to 570 seconds after submission |
| Provider connection test and model refresh | 15 seconds |
| HTTP pool idle lifetime / idle connections per host | 90 seconds / 4 |

Committed streams are not subject to the pre-commit deadlines; they can run
longer while data arrives. The individual read timeout still applies.

## Dashboard and trust boundaries

The Test page offers Chat, SystemOne, and all seven media endpoint modes. Responses and embeddings are API-only, so Embedding combos are not listed there. `POST /test/systemone` uses the
same dashboard CSRF/origin checks, gateway authentication/scopes, traffic
admission, history/capture, and diagnostic wrapper as Test chat. It validates
native state/questions, removes `_csrf`, rejects streaming, and routes only to
SystemOne targets using the real combo strategy/fallback loop. Native response
JSON is returned inside the existing `completion` trace envelope. Browser inputs
and formatted results are page-local; results use textContent, never HTML.
Switching mode clears chat history/results/session identity; pagehide discards
keys and evaluation inputs too. The SystemOne form keeps state/questions after
success or failure for explicit repeat/retry and does not replay past results as
chat messages.

Media uses `POST /test/media/{mode}` (JSON image/video/music/speech) or
`POST /test/media-upload/{mode}` (multipart image-edit/transcription/translation).
Both require a CSRF header, dashboard origin/login checks, gateway authentication,
and a combo target. They remove CSRF fields and reuse `api/media.rs` validation,
scopes, admission, routing, metadata-only history (`source: test`), and complete
binary stream guards. Files remain shared bounded buffers. The original response
bytes/format are preserved, including binary streaming. A dashboard-only base64
JSON diagnostic header is capped at 4096 decoded bytes, dropping whole trailing
attempts and setting a truncation flag; full attempts still go to History when
enabled. No public API format/header is changed.

Browser media controls support native extra fields, voice IDs and file/mask
uploads. Binary audio becomes a page-local Blob only after successful body EOF;
interrupted downloads do not produce a successful preview. Inline base64 images
use local raster Blob previews. CSP permits blob images/media, not arbitrary
remote sources. Remote URLs are validated HTTP(S) links opened explicitly with
noopener/noreferrer; no remote fetch/proxy or bearer forwarding is introduced.
Reset/mode/combo changes pause players and revoke Blob URLs; pagehide aborts
fetches, clears files/options/voices/keys and rejects stale responses. Media never
enters chat history or browser storage; rendered JSON/transcripts use textContent.

`GET /test` renders a combo playground with embedded JavaScript.
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
The `/` Overview is the default dashboard landing and login destination. On page
load it probes SQLite with the same bounded read check as readiness and reads
Linux `/proc/self/status` for current process resident memory (unavailable on
other platforms or read failure). Retained success and failure counts exclude
pending streams; failures include errors, interruptions, and cancellations.
Unavailable database/history reads render unknown totals with a sanitized notice;
the history summary has a two-second deadline. No monitoring worker is used.
It reads retained history and displays four usage summary cards (input/output/
cache-read tokens, USD spend) plus the ten providers ranked by distinct requests.
It performs summary and leaderboard reads in SQL read transactions; it does not
call providers or use a worker. Pending streams are excluded. Only the selected
successful attempt contributes usage, preventing fallback retries from being
double-counted. Cache-read tokens are a subset of input tokens, not additive to
them. Missing metrics remain unknown; coverage counts make partial totals visible.
Only explicitly reported USD costs are summed; non-USD costs and missing costs
are unknown, with no estimated pricing. Clearing retained history clears these
statistics, not lifetime totals.
`/settings` provides browser-local Light/Dark/System selection, persisted history
retention and confirmed clearing, configuration export/import and inline preview, and
read-only sanitized security/system status. Retention updates and immediate
pruning use `BEGIN IMMEDIATE`, serializing with history inserts; inserts read the
cap under their own write lock. Settings never edits deployment secrets or
rewrites environment files. Uptime is tracked by a shared monotonic startup
instant; system status is sampled on page load without a worker.
The shared shell renders page headings and the current navigation section on the
server. One embedded stylesheet supplies light/dark design tokens, reusable
controls, focus indicators, and responsive layout; no frontend build is needed.
`POST /combos/{id}/duplicate` requires dashboard CSRF/origin checks and copies
the combo and its ordered steps in one immediate transaction. A unique copy
name and fresh combo/step IDs are allocated; provider connection IDs are reused.
The list's clipboard control uses embedded JavaScript to copy `combo/<name>`;
it is hidden when the browser's clipboard API is unavailable.

The providers index is a preset/custom tile catalog. Cards show fixed capability
labels (Text, Image, Video, Music, TTS, STT, Decision) with documented preset or
protocol-derived defaults. Multi-capability vendors use one tile and connections.
CSRF/origin-protected tag forms save optional per-provider overrides in SQLite;
these labels never affect protocol dispatch, model scopes, or routing. Custom
overrides cascade on group deletion; preset overrides persist without connections.
Client-side checkbox filters use any-tag matching combined with title-only search.
Assignments are dashboard metadata, not part of configuration transfer.
 `/providers/catalog/{slug}`
and `/providers/groups/{id}` show named connections and an add-connection popup.
The popup uses native HTML popover controls, retaining ordinary POST forms and
CSRF fields without inline scripts or a JavaScript dependency. Browsers without
popover support display the form inline.
Model details also use native popovers with unique connection/model targets;
opening, closing, Escape, and focus handling require no custom dialog script.
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
These protections are independent of the dashboard login. **Dashboard access permits
changing credentials and making outbound connections, including to local hosts.**
Login is required by default regardless of the peer address; only
`ROUTER_DASHBOARD_AUTH=disabled` removes the middleware. The password is either an
Argon2id PHC string in `dashboard_admin` (default) or `ROUTER_DASHBOARD_PASSWORD`,
which stays in process environment/state, never SQLite. With no stored password,
the middleware sends every page to `/setup`, which requires an 80-bit one-time code
generated at startup and logged once; the code proves control of the process and is
discarded when setup stores a password (an `INSERT` on the single-row table, so a
concurrent setup cannot overwrite it). Argon2 runs on blocking threads, at most two
at a time. Environment passwords are compared as domain-separated HMAC fingerprints
in constant time. A password change replaces the hash and clears every session but
the changing browser's. `sparkrouter reset-dashboard-password` deletes the row
offline; the running process reads the hash only at startup. Random 256-bit session cookies are HttpOnly, SameSite=Strict, optionally
Secure; a bounded process-local session map enforces eight-hour expiry and logout
revocation. Restart invalidates all sessions. Bounded per-socket-peer accounting
permits ten failed login, setup-code, or current-password attempts per minute;
each attempt is counted before its (slow) check so concurrent guesses cannot exceed
the limit, and a successful one clears that peer's count. When 1,024 peers are tracked, the oldest unthrottled peer is evicted;
only if every tracked peer is throttled do new peers share one overflow bucket.
Forwarding headers are deliberately ignored.
Reverse proxies should apply their own per-client limits. Gateway API authentication does not
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
unchanged.

`master_key_material` holds envelopes sealed with the master key. `check` is a
constant verified at startup: a mismatched key stops startup instead of failing
per request. Databases created before it are verified once against their stored
credentials (at least one must decrypt) before the check is recorded. HMAC
fingerprints (generated API keys, environment dashboard password, affinity) use a
separate fingerprint key, which is the master key until the first rotation.
`sparkrouter rotate-master-key` runs offline in one `BEGIN IMMEDIATE` transaction:
it verifies the current key, re-encrypts every provider credential and unrevealed
API-key envelope with the same associated data, seals the current fingerprint key
as `fingerprint` and a new `check` with the new key, and aborts without changes if
any envelope fails to decrypt. Stored API-key fingerprints therefore stay valid.
Rotation does not rewrite backups or exports; keep old keys for old backups.

Request middleware generates/propagates `x-request-id` and creates tracing spans.
Routing records combo, provider ID, model, status, latency, and fallback events.
HTTP errors are logged through sanitized categories rather than full Reqwest
errors. Do not enable or introduce credential/header/body dumps in operational logs.
Explicitly opted-in Extended history stores bounded gateway bodies in SQLite only.

## Extension rules

### Local request history and configuration transfer

Logging mode is sampled after authorization/admission, before routing, by
`history_payload.rs`, waiting at most 200 ms for a connection; read failures
disable recording for that request. Request-path SQL is never cancelled after it
starts: SQLx discards a connection whose query future is dropped, and under load
that churn and wasted work collapsed throughput. Deadlines bound only the wait. Off returns the unwrapped response without inserting history. Basic
is the upgrade/default behavior. Extended snapshots gateway JSON and observes
final response data incrementally, copying at most 64 KiB per direction without
changing forwarded bytes/errors or fallback. It excludes headers and intermediate
upstream errors, not secrets supplied in message/tool/extension body content.
Bodies are intentionally unencrypted development data in SQLite/WAL/backups;
Extended requires explicit confirmation in a CSRF/origin-protected Settings form.
The payload insert waits for metadata and conditional SQL cannot resurrect a
cleared/pruned parent. Body finalization/drop is spawned so the body end never
waits on SQLite; that task waits for the shared insert gate,
with bounded best-effort writes and incomplete/truncated flags. Capture reads are
on a dashboard-auth-protected, autoescaped, no-store HTML inspection page, not an
API response. Existing and in-flight captures survive mode changes. Pruning and
clear cascade bodies but are not secure disk erasure. No content enters tracing.

`history.rs` observes routing traces and response bodies without changing upstream
bytes, response envelopes, strategy state, or fallback semantics. JSON responses
carry best-effort usage metadata in internal response extensions. The streaming
observer frames bounded SSE events and reads usage without re-encoding events;
it parses only events whose data contains `"usage"` (a key written with JSON
escapes is not recognized). Malformed observer data never alters forwarding. Terminal body errors record
interruption; dropped bodies record cancellation (or success if `[DONE]` was
already observed). History inserts, streaming finalization (completion, body
error, or drop), and Extended captures are queued without waiting for the
group-commit writer in `db/history_writer.rs`, explicitly requested to lift the
per-write commit ceiling. It is the one background task: it owns the single
history connection, so writes never contend for SQLite's write lock while
holding connections that authentication and routing reads need, and it commits
everything queued since its previous commit in one transaction (multi-row
statements, Extended captures included, one retention prune; at most 1024
writes). History IDs are time-ordered UUIDv7 strings, so every index keyed by
them grows at one edge and prunes from the other; random IDs dirtied a random
page per index on each insert and prune. There is no timer; a
lone write commits at once. The queue is FIFO and each transaction writes
inserts before terminal outcomes and captures, so a stream is never left
recorded as streaming after its outcome was queued. The queue is bounded
(8192 writes, 32 MiB of bodies, times the 1/2/4/8 multiple saved in Settings
and applied to the running writer) and full queues drop writes rather than
block the response. A constraint violation retries that batch one write per
transaction; captures run in a savepoint so their failure keeps the metadata.
Started transactions are never cancelled. Recording failures are logged only as
fixed categories and never fail generation. Startup marks orphaned streaming rows interrupted.
The `request_history` and `request_attempts` tables contain metadata snapshots
only, with no foreign keys to live configuration that could block deletion.
Inserts prune to the configured newest requests (default/maximum 10,000) by deleting only the oldest excess rows, so the cost does not grow with retention; attempts cascade with history
deletion. A new additive migration stores cache-token and cost fields on request
history and provider-slug/group-name snapshots on attempts, captured from the
same SQL configuration snapshot used for each attempt. These identities survive
provider rename/deletion and history remains independent of live configuration.
Older rows and attempts without identity retain unknown values; the dashboard
does not fabricate cache/cost or assign a provider identity per connection.
OpenAI cache-read usage is recognized from `prompt_tokens_details.cached_tokens`,
`input_tokens_details.cached_tokens`, or `cache_read_input_tokens`; Anthropic
cache-read usage is translated from its read-token usage, including streams when
usage events are requested. Cost is accepted only from explicit `usage.cost`
(OpenRouter-style USD) or `usage.cost_usd`; explicitly non-USD costs are ignored.
Missing usage and cost stay NULL/unknown, while explicit zero is valid. Summary
coverage counts surface partial totals rather than implying invoice accuracy.
The dashboard lists 50 rows per page with aggregates and a protected clear action;
Overview aggregates the same retained window.

Configuration transfer uses a strict versioned JSON schema and consistent export
snapshot, excluding all secret material, API keys, cached models, and history.
Preview performs validation; confirmation repeats validation under a write
transaction and inserts fresh IDs without modifying existing rows. Invalid
references or conflicting combo names roll back the entire import. Imported
connections have empty credential blobs and start disabled, regardless of their
exported enabled flag. Endpoints must be reviewed and credentials set before
enabling. Provider groups, including distinct groups sharing an endpoint and empty
groups, and combo strategy/step ordering survive transfer. This is not a database
backup or master-key rotation mechanism. Combo affinity mode and Sticky minutes
are optional version 1 fields (older files default to Off/5 minutes), validated
before writes and preserved on transfer. Runtime hints are never exported.

- Add protocols in `src/providers/` with explicit request/JSON/SSE translation,
  preserving the existing retry and commitment contracts.
- Add new database changes as SQLx migrations, retaining ordering constraints.
- Add dashboard authentication on the dashboard router, independently of the
  gateway's bearer middleware.
- Prefer configuration over vendor-specific code; avoid adding new services or
  generic abstractions without an actual requirement.

See [AGENTS.md](AGENTS.md) for contributor invariants and [TESTING.md](TESTING.md)
for the regression checks that protect these boundaries.
