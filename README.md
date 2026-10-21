# SparkRouter

A small self-hosted AI API gateway: one Rust executable, SQLite, and a server-rendered dashboard. Add providers, arrange provider/model pairs into ordered **combos**, and call them using Chat Completions, SystemOne, or image/video/music/speech APIs.

## Documentation

- [Build and deployment](BUILD.md)
- [Architecture and security boundaries](ARCHITECURE.md)
- [Testing and manual acceptance](TESTING.md)
- [Media APIs, supported providers, and OmniRoute audit](MEDIA.md)
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

**Save the generated master key securely and reuse it on every restart.** Starting with a different key fails with a mismatch error; to change it, rotate it as described in [Master-key rotation](#master-key-rotation). Back up the key separately from the database. The executable reads process environment variables; `.env.example` is a reference, not an automatically loaded configuration file.

Open **http://127.0.0.1:14673/** for the Overview dashboard. The dashboard always requires a password, including on localhost. On first start the server logs `dashboard setup required: open http://…/setup and enter dashboard setup code XXXX-XXXX-XXXX-XXXX`; open `/setup`, enter that one-time code, and choose the admin password (at least 12 characters):

The Overview is the default login destination and sidebar entry. It shows SQLite read connectivity, successful and failed request counts, and current process resident memory (Linux; unavailable on other platforms). Status is sampled on page load; reload to refresh. Failed counts include errors, interrupted streams, and cancellations, not authentication/validation/traffic-limit rejections. Pending streams are excluded. Database/history failures show unavailable values instead of fabricated zeros; connectivity does not prove write access. It also summarizes retained request usage in four cards (input/output/cache tokens and USD spend; the request total is the successful plus failed counts) and ranks the top ten providers by requests. Use the sidebar to switch between **Overview**, **Providers**, **Combos**, **API keys**, **Test**, **History**, and **Settings**; **Log out** sits at its foot. On smaller screens navigation becomes one horizontally scrollable row above the page. With JavaScript enabled, deleting a provider, connection, or combo, revoking a key, and clearing history ask for browser confirmation first, and history/key creation times are shown in the browser's local time (hover for the stored UTC value).
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
3. Open **Combos**, create `coding` with category **Text**, and add provider/model steps in preference order.
4. Choose a **provider**, then one of its **connections**, for each step and use the up/down
   buttons to change fallback order. Enter model IDs exactly as your upstream
     accepts them. Each combo step targets one connection; it does not implicitly
     rotate or retry other keys within that provider.

Use **Edit** immediately before **Remove** to open a step's inline connection/model editor. Saving
keeps its ID and fallback position; refreshed model suggestions are optional.
Without JavaScript, use the **Edit step** disclosure below the row; all connection
options remain visible and ordinary form submissions still work.

Choose **Priority**, **Round robin**, or **Random** when creating or editing a
combo. Existing combos default to Priority. The Combos list shows its strategy
and cache-awareness mode.

### Category-specific combos

Each combo has one category: **Text**, **Decision / SystemOne**, **Image**,
**Video**, **Music**, **TTS**, or **STT**. The Combos page has a category filter;
creation/editing shows the category alongside the routing strategy. `/test`
only offers combos matching its selected mode and disables sending when none
exist. Image edits share Image combos; audio translation shares STT combos.

The backend rejects wrong-mode combo requests with `400` before quota admission
or strategy rotation. Step creation/editing validates protocol compatibility,
explicit cached output modalities, and speech input modalities when available.
Image input alone does not classify a vision LLM as an image generator.
Provider capability tags remain dashboard metadata, not these classifications.
The step picker's suggestions only include matching declarations, known catalog
entries, explicit capability/endpoint metadata, or models from single-category
protocols such as SystemOne.
Unknown models on multi-category providers are not offered in every category.
Audio output alone does not distinguish Music from TTS; those models need a
matching declaration or clear speech input/output metadata. Manual entry remains
available when discovery cannot identify the category.

Automatic classification uses **exact, provider-scoped documented IDs**, not
name prefixes or substring guesses. A small built-in catalog covers selected
OpenAI, MiniMax, ElevenLabs, xAI, Deepgram, and fal.ai models. OpenAI IDs are only
recognized on `api.openai.com`; native vendor protocols also support custom
mirrors. A custom OpenAI-compatible host reusing the same ID does not inherit
OpenAI's classification. The catalog does not add connections, fetch models,
prove account access, or advertise OpenAI Sora.

Builds also import category evidence from **models.dev** using
`https://models.dev/api.json?type=all`, with a bundled snapshot fallback. Matches
require the preset's exact endpoint and protocol plus the exact upstream model
ID; custom hosts do not inherit vendor classifications. Image/video output is
recognized; audio-only input with text-only output indicates STT. Text-only
output plus text input and explicit tool support provides conservative chat
evidence (text modalities alone can also describe embeddings). Ambiguous audio
output and models without sufficient evidence remain manually classifiable.
Hand-reviewed catalog entries still win over imported entries, and upstream
endpoint/modality metadata retains its existing precedence. The import does not
populate refreshed model lists or change direct routing. Only categories are
imported; pricing, limits, and other model information are not added to the UI.
No runtime network lookup is added. See [build controls](BUILD.md#build-time-modelsdev-categories)
for offline/reproducible builds and forcing a fresh import despite Cargo caching.

Explicit `supported_endpoints` / `supportedEndpoints` discovery metadata is
also recognized (chat, images, videos, music, speech, transcription/translation,
and SystemOne, including common `/v1/` paths and hyphenated aliases). Endpoint
metadata takes precedence over advisory catalog entries; contradictory modality
metadata still blocks the model. An `embeddings` endpoint declaration is evidence
for the Embedding category; unknown/non-gateway-only endpoints never silently
become Text. The same policy applies to suggestions,
step mutations, imports, and combo routing. Existing wrong-category declarations
fail closed and must be corrected, not silently reassigned. Imports revalidate
catalog entries; cached endpoint metadata is not transferred.

Adding an unclassified/custom ID explicitly
**declares that connection/model in the combo's category**: make sure it really
supports that API. The same connection/model cannot be used in conflicting
categories while referenced by any combo. Remove all references before
reclassifying it. A missing model list still permits manual IDs. Refresh/protocol
changes are revalidated before any combo attempt and fail closed if incompatible.
Remove all steps before changing a combo's category; category changes and step
mutations are transactional. Duplication and configuration transfer preserve
categories. `/v1/models` adds `category` to combo entries; direct routes are
unchanged.

Migration 0019 preserves other installations' existing combos: SystemOne-only
chains become Decision; remaining chains default to Text. Legacy media/mixed
chains must be rebuilt in their correct categories; incompatible chains cannot
route. It does not delete configuration automatically.

### Model classification and connection refresh diagnostics

Each refreshed model card shows a category icon immediately left of Copy, the
same size as the action icons, with an accessible category label. Its Info popup
shows the category and evidence source: upstream metadata, built-in catalog,
models.dev, manual declaration, or single-category protocol. Unclassified models
use a question-mark icon; conflicting evidence uses a warning icon. Contradictory known
evidence/declarations show **Needs review**, not a guessed category. The Info
popup explains which saved declaration, catalog category, connection protocol,
or upstream endpoint/modality metadata caused the conflict. Classification
controls now live inside each model's Info popup, alongside the embedded catalog
source/date. Select a category and **Save**, or select **Default** and Save
to remove the saved declaration and use automatic metadata/catalog/protocol
evidence (or Unclassified if none exists). Known capability constraints still
apply. A reset is blocked if its automatic default would invalidate any
referencing combo; unknown referenced models require an explicit declaration.
Remove referencing steps before changing their declared category. Known automatic
models can route after reset without a saved declaration, but changed metadata is
still revalidated before every combo request. Unlisted IDs can still be declared
through combo step entry. Declarations do not add models to direct discovery or
prove account access.

Connection model controls show the last successful refresh, last attempt,
discovered count, and sanitized refresh failure. Failed refreshes keep the last
successful list and the error survives page reloads. Lists older than 24 hours
are labeled stale; this is advisory and does not disable routing. Successful
refresh clears the error. Refresh remains user-triggered; no background polling
or generation calls are introduced.

History's attempts disclosure includes the snapshotted strategy, strategy-selected
and actual starting step, affinity mode and hit/miss/unavailable-target result,
and an execution-ordered attempt timeline with sanitized failures and latency.
Retryable HTTP failures retain their numeric status; transport/invalid-response
failures have a generic reason. No prompts, session IDs, URLs, credentials, or
raw upstream errors are recorded in these diagnostics. Selection is not stream
completion: the final request outcome still reports interrupted/cancelled bodies.
Older records do not reconstruct routing settings from today's configuration.
Logging Off skips these new diagnostics too; retention/clear cascade to them.

### Per-combo cache-awareness

Each combo has one **Cache-awareness** dropdown:

| Mode | Starting-target preference |
| --- | --- |
| **Off** (default) | Use the combo's Priority, Round robin, or Random strategy unchanged |
| **Basic** | Reuse the last successful connection/model for the same explicit session ID |
| **Smart** | Same as Basic; without an explicit ID, fingerprint the initial consecutive system/developer messages, first user message, and tool definitions |
| **Sticky** | Reuse this caller's last successful connection/model for this combo, regardless of session or prompt |

Set **Affinity timeout (minutes)** on that combo to **1–1440 minutes** (default 5).
Basic, Smart, and Sticky hints all expire that long after their last success;
match it to how long your upstream keeps prompt caches warm. Combos already in
Basic or Smart mode before this setting applied to them were set to 30 minutes,
their previous fixed expiry. Successful requests renew hints; streams establish/renew them only when the body
finishes successfully, not when response headers arrive. Failures or cancellation
do not establish a new hint. On a miss, expiry, or unavailable target, the normal
strategy chooses the start. Round robin still advances exactly once per request,
even on affinity hits. An affinity hit starts configured cyclic fallback at the
remembered step, trying each enabled step at most once; permanent errors still
stop routing, and streaming never switches after commitment.

For Basic/Smart, prefer a gateway-only header:

```http
X-SparkRouter-Session-Id: client-generated-stable-chat-id
```

Supported header precedence is `X-SparkRouter-Session-Id`,
`X-SparkRouter-Conversation-Id`, `session-id`, then `X-Session-Id` (case-insensitive).
If the selected header is absent/blank, string `session_id`, `conversation_id`,
or `chat_id` fields in the request body, then in its `metadata` object, are checked
in that order. Blank/null body fields mean absent; other types, control characters,
or IDs longer than 256 bytes are rejected in Basic/Smart. IDs are trimmed and
case-sensitive. Header IDs are not forwarded upstream. Body extension fields are
preserved by the OpenAI adapter, so use a header if your upstream rejects them.
No identifiers are extracted from prompt text, tool-call IDs, `user`, or
`x-request-id`. The Test chat page generates a page-local session ID and replaces
it on New chat or combo changes.

Hints are isolated by combo and generated gateway-key ID. Clients sharing the
environment key share one namespace, and unauthenticated clients share another;
use separate generated keys for caller isolation. Smart fingerprints preserve
exact message/tool content but ignore JSON object-key ordering. Identical initial
prompts can share a hint; this is not proof that they are the same conversation.
Without a first user message, Smart falls back to normal selection.

This is **placement affinity for upstream prompt caches**, not response caching,
cache-hit prediction, or a guarantee of lower bills. It works with logging Off
and never reads saved request bodies. Only keyed fingerprints and routing IDs
are retained in a bounded process-local map (up to 4096 hints), not raw IDs,
prompts, or API secrets. Hints reset on restart, are invalidated by combo/step
edits, and may be evicted at capacity. Concurrent cold requests can select
different targets before either finishes. Mode/timeout configuration persists in
SQLite and survives duplication/export/import; new and upgraded combos default
to Off.

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
The **Mode** selector offers **Chat**, **SystemOne**, **Image**, **Image edit**,
**Video**, **Music**, **TTS / Speech**, **STT / Transcription**, and **Audio translation**. For
SystemOne, select a Decision-category combo containing SystemOne connections, edit **State** as a
JSON string/object/array and **Questions** as a JSON object, then click
**Evaluate**. A Noul example is prefilled; Choice and Score questions and structured
instructions/criteria are supported too. Results display the native model,
answers, usage, and extension fields as formatted JSON, with the same routing
attempt details. Inputs remain available for repeat evaluations; **Clear results**
clears results and starts a new session. Mode/combo changes clear previous results
and chat context. Inputs, results, and gateway keys are never saved in browser
storage and are discarded when leaving the page.

For media, choose a compatible combo, enter a prompt or speech text, and supply
files for image editing/transcription/translation. Image edits optionally accept
a mask. Enter a provider-specific voice for speech where required. **Advanced
media options** accepts a JSON object for native fields such as size, duration,
lyrics, language, and response format. The combo and file controls own their
reserved fields. Inputs stay available after success/failure for deliberate
repeat tests; media results never enter chat context.

Inline base64 images display a local preview/download; binary audio displays a
player/download after its body finishes. Browser codecs vary; raw PCM may need
downloading. Remote image/video/music URLs are explicit no-referrer links, never
automatically loaded or proxied through the gateway. Video may take several
minutes, and leaving the page does not guarantee upstream job cancellation or
prevent charges. Routing diagnostics use a bounded header for media, so audio
stays binary/streaming; any omitted tail attempts are identified, with full
metadata in History when logging is enabled. Media remains metadata-only even
in Extended logging.

The playground requires JavaScript. Chat/evaluations are non-streaming; media
uses the same adapters and stream guards as the API. Conversation history,
media, and file selections stay page-local. Reset/mode/combo changes discard
results and release preview URLs; leaving the page also clears keys and inputs.

```sh
curl http://127.0.0.1:14673/v1/chat/completions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"combo/coding","messages":[{"role":"user","content":"Hello"}]}'

# Streaming: add "stream":true to the JSON and use curl -N.
curl http://127.0.0.1:14673/v1/models \
  -H "Authorization: Bearer $ROUTER_API_KEY"
```

Set an OpenAI-compatible client's base URL to `http://127.0.0.1:14673/v1` and model to `combo/coding`. Generate a key under **API keys**, or set `ROUTER_API_KEY`. The environment key is unrestricted; generated keys can be unrestricted or scoped to selected model IDs. If neither is configured, omit Authorization (clients requiring a key may send any placeholder).

### Per-key traffic limits

Generated API keys optionally support **Requests per minute** (1–1,000,000) and
**Maximum concurrent requests** (1–10,000). Set these when generating a key or
under **Edit access / expiry / limits**; blank means unlimited. Existing keys
remain unlimited, as does `ROUTER_API_KEY`. Limits apply to chat completions, SystemOne evaluations, media generation/transcription,
and test chat together; `/v1/models` is exempt.

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

**Regenerate** replaces a key's secret and shows the new one once. The key keeps
its name, allowed models, expiry, limits, and routing affinity namespace; the
previous secret gets `401` from the moment the new one is issued. Requests that
already started with the old secret are not interrupted. Regenerating does not
extend an expired key; edit its expiry separately.

`/v1/models` lists only models allowed by the supplied key. A valid key calling a
disallowed model gets `403`; an expired, revoked, or invalid key gets `401` while
authentication is enabled. Test chat uses the same restrictions. Authorization
is checked at request start: expiry/revocation/policy edits do not interrupt
already-started requests or streams. Expired keys still keep authentication
enabled until explicitly revoked. Revoking the last key reopens the API only
when `ROUTER_API_KEY` is also unset.

A refreshed model can also be called directly using **`<provider-slug>/<upstream-model-id>`**, with no combo:

```sh
curl http://127.0.0.1:14673/v1/chat/completions \
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
# Automatically load a prepared .env; never replace an existing master key.
bash scripts/sparkrouter.sh start
bash scripts/sparkrouter.sh health
bash scripts/sparkrouter.sh stop
```

```powershell
.\scripts\sparkrouter.ps1 start
.\scripts\sparkrouter.ps1 health
.\scripts\sparkrouter.ps1 stop
```

**Windows background stop is forced and interrupts active requests.** For
graceful Windows shutdown, use `run` instead of `start` and press Ctrl+C.
Scripts automatically load `.env` from the caller's working directory if it exists.
Use `--env-file FILE` / `-EnvFile FILE` to select a different file. Without a `.env`,
they use the inherited process environment. The executable itself still reads only
process environment variables.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `ROUTER_MASTER_KEY` | **required** | Base64-encoded 32 random bytes; AES-256-GCM master key |
| `ROUTER_DATABASE_URL` | `sqlite://router.db` | SQLite URL; database and schema created on startup |
| `ROUTER_API_KEY` | unset | Optional bearer key protecting `/v1/*`; an explicitly empty value is rejected. Dashboard-generated keys also authorize `/v1/*` and, once any exist, require authentication even when this variable is unset |
| `ROUTER_BIND` | `127.0.0.1:14673` | IP address and port |
| `ROUTER_DASHBOARD_PASSWORD` | unset | Optional fixed admin password instead of the one chosen at first-run setup; 16–1024 bytes, not blank. It cannot be changed in the dashboard |
| `ROUTER_DASHBOARD_AUTH` | `required` | `required` or `disabled`. Disable only behind an authenticating reverse proxy; cannot be combined with `ROUTER_DASHBOARD_PASSWORD` |
| `ROUTER_DASHBOARD_SECURE_COOKIE` | `false` | Set `true` for HTTPS deployment; marks dashboard session cookies Secure. Must be `true` or `false` |
| `RUST_LOG` | `info` | `tracing` filter |

The parent database directory must already exist and be writable. SQLite WAL/SHM sidecar files are normal while running. For backups, stop the application first or use SQLite's online backup facility rather than copying an active database file alone. Change configuration through the dashboard while the gateway runs: it caches request-path configuration (keys, combos, connections, logging mode) and does not notice edits made to the database by another process until it restarts or commits a change itself.

## Providers and protocols

The Providers page has a **Favorites** section above **All providers**. Use the
star on any preset or custom provider to favorite/unfavorite it. **Anthropic,
OpenAI, and OpenRouter** are favorites by default. Explicit unfavorites persist
across restarts; no connection or API key is required to favorite a preset.
Favorites remain listed under All providers, and name/tag filters apply to both
sections. Providers are sorted A–Z by name (case-insensitively), with custom
providers interleaved with presets. These are shared dashboard preferences stored in SQLite, not routing
priorities or per-browser settings. They are included in full database backups,
not configuration export/import. Deleting a custom provider removes its favorite
preference too.

Provider cards display capability tags: **Text, Image, Video, Music, TTS, STT,
and Decision**. Defaults reflect documented capabilities for supported media presets; other chat
providers default to Text and SystemOne to Decision. Open any preset or custom
provider, check its **Provider tags**, and click **Save tags** to assign multiple
tags or clear them all. Tags are organizational labels, not model/account access
checks or routing permissions. Media APIs are enabled by their protocol adapters,
not by changing tags. Multi-capability services share one provider card and key.
See [Media APIs](MEDIA.md) for supported integrations, request examples, limits,
and the OmniRoute source audit.

Checkbox filters to the right of the Providers search bar match **any** checked
tag and combine with the name search. No checked tags means show all. Filtering
requires JavaScript and resets on navigation/reload; tag assignments persist in
SQLite across restarts. These dashboard-only assignments are included in full
database backups, but not configuration export/import (imported providers use
their protocol defaults).

### Responses API and embeddings

`POST /v1/responses` passes OpenAI Responses requests through like chat: only
`model` is replaced, unknown fields are preserved, and upstream JSON or SSE bytes
are forwarded unchanged. It uses **Text** combos and direct IDs, and only OpenAI-
and xAI-protocol connections are eligible (Anthropic, MiniMax and SystemOne
steps are skipped, as other wrong-protocol steps are). Fallback follows the chat
rules: retryable failures before the first streamed byte try the next target,
and nothing falls back afterwards. Many OpenAI-compatible vendors implement only
chat completions, so on `/v1/responses` and `/v1/embeddings` an upstream 405 or
501 (endpoint not available there) also tries the next target; 400 and 422 still
stop the chain. A stream is complete only at a
`response.completed`, `response.incomplete`, or `response.failed` event; an
upstream EOF before that ends the body as interrupted. Usage from the final
response is recorded in history. Stateful features such as
`previous_response_id`, `conversation`, and stored responses live at the upstream
that created them, so a fallback to a different provider usually fails for those
requests; use combo session affinity to keep a conversation on one target. Only
`POST /v1/responses` is routed: retrieving, cancelling, or deleting a stored
response by ID has no model to route by and is not proxied.

```sh
curl http://127.0.0.1:14673/v1/responses \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"combo/coding","input":"Write a haiku about routers.","stream":true}'
```

`POST /v1/embeddings` requires an **Embedding** combo (or a direct ID) with
OpenAI-protocol connections, which covers OpenAI and most OpenAI-compatible
servers. The request needs `input` and cannot stream; everything else
(`dimensions`, `encoding_format`, vendor fields) passes through, and the vector
response is returned unchanged with its usage recorded. Embedding models are not
in the bundled catalogs, so adding one to an Embedding combo declares it there;
a model documented as a chat model is refused. Embedding combos never serve
chat, and Text combos never serve embeddings.

```sh
curl http://127.0.0.1:14673/v1/embeddings \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"combo/vectors","input":["first text","second text"]}'
```

### SystemOne evaluations (TypeSafe AI)

The **TypeSafe AI** preset uses `https://api.typesafe.ai/v1` with the native
**SystemOne** protocol and Bearer authentication. Custom SystemOne-compatible
providers can select that protocol too. Add a connection, refresh models for
direct routing, or create a combo with upstream model IDs such as `jev-latest`.

```sh
curl http://127.0.0.1:14673/v1/systemone \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"typesafe/jev-latest","state":"Help! My payouts are failing.","questions":{"urgent":{"type":"noul","instructions":"Does this convey urgency?"}}}'
```

Use `combo/<name>` instead for combo routing. `/v1/systemone` requires a
Decision-category combo with SystemOne connections; chat requires a Text combo.
Mixed-category combos are rejected rather than silently filtering incompatible
steps.
SystemOne forwards the original evaluation JSON with only `model` replaced and
returns native `model`, `answers`, and `usage` JSON without chat translation.
Noul, Choice, Score, structured instructions/criteria, and unknown extension
fields are preserved. The gateway validates the top-level model, state, and
questions shape; question-specific validation belongs to the upstream. Streaming
is not supported (`stream: true` is rejected). Authentication, exact public-ID
scopes, per-key traffic limits, history, deadlines, and once-each ordered fallback
apply just as for chat. Upstream 401/429/529 can fall back; 422 stops the chain.
SystemOne model refresh reads `{ "models": [{ "name": "jev-latest", ... }] }`.
`/v1/models` includes these qualified IDs alongside chat models; it is a shared
catalog, not a guarantee that every model supports every endpoint. The Test page's **SystemOne** mode sends native evaluations through
`POST /test/systemone`; its Chat mode remains chat-only. Basic session affinity and Sticky work for evaluation combos;
Smart without a session ID has no chat-message fingerprint and uses the strategy.


Vendors sharing a protocol use the same adapter through configuration; native media protocols have explicit translators. The dashboard includes OmniRoute's API-key chat presets that use the **OpenAI** or **Anthropic** adapters, including OpenRouter, Groq, DeepSeek, Together, and Fireworks. Local OpenAI-compatible servers use the **OpenAI** protocol too. Include any required API prefix (usually `/v1`) in the base URL. Trailing slashes are normalized; URL credentials, query strings, and fragments are rejected. HTTP is supported for local servers. An empty API key is allowed for unauthenticated local servers.

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

The connection test normally sends `GET {base_url}/models` with the saved credentials. It verifies that endpoint, not model access or generation. **Refresh models** stores that list. Deepgram and ElevenLabs use their native list shapes; MiniMax/ElevenLabs lists are supplemented with documented media IDs. fal.ai refresh uses a labeled starter catalog instead of a live discovery/access check, and its connection probe asks for a generation test. See [media discovery](MEDIA.md#availability-and-discovery). Model cards show and copy the provider-prefixed **public ID** (Copy uses the modern Clipboard API on HTTPS/localhost, with a legacy fallback for plain HTTP LAN access); model details also show the **upstream ID** used by combo steps. Cards support search and show only a compact context-size tag (for example, `128k ctx` or `1.05M ctx`, rounded to at most two decimal places); output limits, pricing, and free-model status stay in the info popup. Combo suggestions retain raw upstream IDs, and you can still type any model. Some servers do not implement model listing; a failed refresh does not mean their chat endpoint is unusable. Save changes before testing them.

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
- One shared pooled HTTP client uses a 10-second connect timeout, 120-second idle read timeout, and 90-second idle pool lifetime. Each attempt has 120 seconds to prepare its response. The pre-commit fallback ceiling is 120 seconds per eligible target plus 60 seconds (180 seconds for one target), capped at 10 minutes, so a hanging target does not silently skip the rest of a chain; media uses 600 seconds per target plus 10, capped at 30 minutes. Targets left when the cap is reached are skipped. Committed streams can run longer while data continues to arrive. Redirects are rejected to protect credentials.
- Requests are limited to 4 MiB, non-stream responses to 16 MiB, and SSE events to 1 MiB. Streaming is incremental and backpressured, without buffering a complete generation. A clean upstream EOF without the protocol's completion marker is treated as an interrupted stream.

## Security and deployment

Keys are encrypted using AES-256-GCM with a fresh random nonce and authenticated provider ID. The database stores versioned ciphertext; the master key is never stored there. Secret values are wrapped with `secrecy` and key/decryption buffers use `zeroize`. Normal logs include routing IDs, model, latency, status, and fallback events; they omit credentials and upstream bodies.

The dashboard requires a single-admin login at `/login` by default, including on loopback. Until a password exists, every dashboard page redirects to `/setup`, which needs the one-time setup code the server logs at startup; the code proves control of the server, so whoever reaches an unconfigured dashboard first cannot claim it. The chosen password (12+ characters) is stored as an Argon2id hash in SQLite and can be changed in **Settings**, which signs out every other session. `ROUTER_DASHBOARD_PASSWORD` instead fixes the password in the environment (no setup, not changeable in the dashboard). If you forget a stored password, stop the server, run `sparkrouter reset-dashboard-password`, and use the new setup code printed at the next start. `ROUTER_DASHBOARD_AUTH=disabled` turns the login off for deployments behind an authenticating reverse proxy; the server logs a warning when it is off. Sessions expire after eight hours and are revoked by logout, a password change, or restart; cookies are HttpOnly and SameSite=Strict. Set `ROUTER_DASHBOARD_SECURE_COOKIE=true` behind HTTPS. There are at most 128 active sessions, and ten failed login, setup-code, or current-password attempts per minute trigger a temporary limit per socket-peer IP; a successful attempt clears that peer's count. Forwarded headers are not trusted; behind a reverse proxy, add a per-client login limit there because clients share the proxy's peer IP. There are no user accounts. Do not expose dashboard passwords over public HTTP.

### Master-key rotation

The database records an encrypted check value, so starting with the wrong `ROUTER_MASTER_KEY` fails instead of producing undecryptable credentials. To rotate the key, stop the server, generate and save a new key, then run:

```sh
ROUTER_NEW_MASTER_KEY="$(openssl rand -base64 32)"   # save this value first
ROUTER_MASTER_KEY=<current key> ROUTER_NEW_MASTER_KEY=<new key> sparkrouter rotate-master-key
```

Every provider credential and unrevealed generated-key envelope is decrypted with the current key and re-encrypted with the new one in one transaction. If any stored credential does not decrypt, nothing changes. Afterwards set `ROUTER_MASTER_KEY` to the new key and start the server; the old key no longer starts it. Generated gateway API keys keep working: their fingerprints cannot be recomputed without the keys, so the first rotation stores the fingerprint key sealed with the new master key. Regenerate API keys if you also want to retire those fingerprints. Backups made before the rotation still need the old key. The helpers wrap this as `rotate-key`, and Docker as `docker run --rm -v sparkrouter-data:/data -e ROUTER_MASTER_KEY -e ROUTER_NEW_MASTER_KEY sparkrouter rotate-master-key`.

`ROUTER_API_KEY` and dashboard-generated API keys protect gateway endpoints only; a dashboard session does not authorize API requests. Generated keys support model allowlists and expiry, are shown once, and are authenticated by a master-key fingerprint; the temporary encrypted reveal envelope is cleared on first viewing. Dashboard forms retain CSRF and origin checks; scripts/styles are local embedded assets. Provider URLs intentionally permit private/local endpoints, so dashboard access grants control over outbound connections. TLS termination belongs at your reverse proxy.

### Settings

**Settings** (`/settings`) offers browser-local Light/Dark/System appearance,
request-history retention (1–10,000 records, default 10,000), a confirmed clear
history action, configuration export/import with inline preview, a dashboard
password change form (stored passwords only; it requires the current password and
signs out other sessions), and read-only authentication and system status (version, uptime, database read connectivity, Linux resident
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

**History** (`/history`) shows routed API and test-chat requests, outcomes, ordered attempts, connection/model names, latency, and token counts when upstream usage is available. Authentication, permission, and boundary-validation rejections are not recorded. **Basic** (default) stores metadata only. **Off** records no new routed history. **Extended**, explicitly confirmed in Settings, also stores sensitive chat/SystemOne request/response body captures. Media requests always remain metadata-only, including binary audio completion/interruption tracking; uploads, prompts, output bodies, and signed media URLs are not captured. Retention defaults to the newest 10,000 requests (a lower cap can be selected in Settings), with pagination, aggregate statistics, and a CSRF-protected clear action. Streaming history distinguishes completion, interruption, and cancellation; a successful HTTP status alone is not a completed generation. Streaming token counts require upstream usage events (for OpenAI-compatible servers, request `stream_options: {"include_usage": true}` if supported). Unknown usage remains unknown, not zero. Recording is best-effort; inserts do not delay response commitment and finalization is bounded. History has its own single SQLite writer connection, separate from the pool that serves authentication, routing, and the dashboard, so heavy traffic cannot starve those reads. Requests queue history writes in memory, and one writer task commits everything queued since its last commit in a single transaction. The queue holds at most 8,192 writes (and 32 MiB of Extended bodies) by default; Settings can raise both to 2×, 4×, or 8× without a restart, which only uses more memory while the disk is stalled. Beyond that a write is skipped with a `request history write unavailable` warning (or `request history finalization unavailable` / `request payload write unavailable`). A started transaction always completes. Shutdown waits up to 10 seconds for queued history to commit. Reading the logging preference waits at most 200 ms for a database connection (failure disables recording for that request); database write failures do not fail generation. The history writer commits with SQLite `synchronous = NORMAL`: in WAL mode this cannot corrupt the database, but the newest history rows can be lost on power failure or OS crash. Providers, keys, combos, and settings keep full synchronous commits. Streaming and Extended body finalization are queued too, so the end of a response never waits on SQLite; the queue is ordered, so a fast completion is not left recorded as still streaming. An interrupted process may lose final metadata; unfinished streams are marked interrupted on restart.

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
test-chat requests; pending streams are excluded. Its four usage cards show
input, output, cache-read, and USD spend, with a top-ten provider
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

**Settings → Backup & restore** (`/settings`) exports versioned JSON containing provider groups, named connections, and combos with ordered steps/strategies. It excludes provider credentials, gateway API keys, cached model lists, and history. This is configuration transfer, **not a complete backup**. Paste JSON to preview it, then confirm import. Imports are validated again and applied in one transaction with fresh IDs; existing configuration is never overwritten, and conflicting combo names reject the entire import. Combo affinity modes and
Combo categories are included; older version 1 files without a category default
to Text and must still pass category/protocol validation. Conflicting model
classifications within a document are rejected before import. Unused model
classifications are not exported. Affinity timeouts are included; older version 1 files without those fields default
to Off and 5 minutes. Distinct and empty custom groups are preserved. **All imported connections start disabled**: review endpoints, enter required provider keys, enable the connections, and refresh models for direct-model routing. Exported URLs and names may still be sensitive; share files carefully.
Group slugs are exported too. Version 1 documents without a slug remain accepted;
imports preserve requested slugs when available and add a suffix on conflicts.
Check the imported provider's displayed slug before updating client model IDs.

One import is limited to 2 MiB of JSON, 100 explicit groups, 500 connections,
200 combos, 50 steps per combo, and 2,000 total steps (the encoded form must
also fit the 4 MiB HTTP body limit). Split larger configurations into smaller
documents with their required references. Ungrouped custom connections receive
their own new provider groups so they remain manageable in the dashboard.

The test page itself is a dashboard page, but `POST /test/chat` and
`POST /test/systemone`, `POST /test/media/{mode}`, and
`POST /test/media-upload/{mode}` additionally require the same gateway authentication as `/v1/*`, plus dashboard CSRF and
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
From the host: `curl --fail http://127.0.0.1:14673/readyz`. The minimal Docker image
does not contain curl; configure probes externally or supply your own probe tool.

### Container deployment (Docker and Podman)

A prebuilt image is published to the GitHub Container Registry by GitHub Actions ([`.github/workflows/container.yml`](.github/workflows/container.yml)):

```
ghcr.io/lutvzn/sparkrouter:latest          # latest master build
ghcr.io/lutvzn/sparkrouter:sha-<commit>    # a specific master commit
ghcr.io/lutvzn/sparkrouter:0.1.0           # release, from git tag v0.1.0
ghcr.io/lutvzn/sparkrouter:0.1             # newest 0.1.x release
```

Pull requests build the image without publishing it. To publish a release, push a `vX.Y.Z` tag matching the `Cargo.toml` version.

It is a `linux/amd64` image of about 15 MB: a statically linked binary on `distroless/static`, with no shell or package manager, running as UID/GID 10001. Inside the container the server binds `0.0.0.0:14673` and stores its database at `/data/router.db`; mount a volume at `/data` to keep it. Pin a version tag (or digest) for production so upgrades are deliberate.

#### 1. Prepare configuration

Generate the master key **once** and store it safely. Losing it makes stored provider keys unrecoverable, and a different key cannot decrypt an existing database.

```sh
mkdir -p ~/.config/sparkrouter
umask 077
cat > ~/.config/sparkrouter/sparkrouter.env <<EOF
ROUTER_MASTER_KEY=$(openssl rand -base64 32)
ROUTER_API_KEY=replace-with-a-long-random-gateway-key
EOF
```

On first start, read the dashboard setup code from the container log (`docker logs sparkrouter`) and open `/setup`. Optionally add `ROUTER_DASHBOARD_PASSWORD` to fix the password instead (and `ROUTER_DASHBOARD_SECURE_COOKIE=true` behind HTTPS). Do **not** copy `ROUTER_BIND` or `ROUTER_DATABASE_URL` from `.env.example`: its loopback bind would make the container unreachable from the host. Env files are plain `KEY=value` lines without quotes.

#### 2a. Docker

```sh
docker pull ghcr.io/lutvzn/sparkrouter:0.1.0
docker volume create sparkrouter-data
docker run -d --name sparkrouter --restart unless-stopped \
  -p 127.0.0.1:14673:14673 \
  -v sparkrouter-data:/data \
  --env-file ~/.config/sparkrouter/sparkrouter.env \
  --stop-timeout 30 \
  ghcr.io/lutvzn/sparkrouter:0.1.0
```

Or with Docker Compose (`compose.yaml`):

```yaml
services:
  sparkrouter:
    image: ghcr.io/lutvzn/sparkrouter:0.1.0
    restart: unless-stopped
    ports:
      - "127.0.0.1:14673:14673"
    volumes:
      - sparkrouter-data:/data
    env_file: ./sparkrouter.env
    stop_grace_period: 30s
volumes:
  sparkrouter-data:
```

Place `sparkrouter.env` (from step 1) next to `compose.yaml`, then run `docker compose up -d`.

#### 2b. Podman

Rootless Podman works without changes:

```sh
podman pull ghcr.io/lutvzn/sparkrouter:0.1.0
podman volume create sparkrouter-data
podman run -d --name sparkrouter \
  -p 127.0.0.1:14673:14673 \
  -v sparkrouter-data:/data \
  --env-file ~/.config/sparkrouter/sparkrouter.env \
  --stop-timeout 30 \
  ghcr.io/lutvzn/sparkrouter:0.1.0
```

To run it as a service that starts at boot, use a Quadlet unit at `~/.config/containers/systemd/sparkrouter.container`:

```ini
[Unit]
Description=SparkRouter LLM gateway

[Container]
Image=ghcr.io/lutvzn/sparkrouter:0.1.0
ContainerName=sparkrouter
PublishPort=127.0.0.1:14673:14673
Volume=sparkrouter-data:/data
EnvironmentFile=%h/.config/sparkrouter/sparkrouter.env
StopTimeout=30

[Service]
Restart=always

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user start sparkrouter
loginctl enable-linger "$USER"   # keep user services running after logout/boot
```

**Bind mounts instead of named volumes:** the directory must be writable by container UID 10001. With Docker, `sudo chown 10001:10001 ./data`. With rootless Podman, append `:U` (`-v ./data:/data:U,Z`) or run `podman unshare chown 10001:10001 ./data`. `Z` relabels for SELinux hosts.

#### 3. Verify, operate, and upgrade

```sh
curl --fail http://127.0.0.1:14673/readyz   # {"status":"ready"}
docker logs -f sparkrouter                  # or: podman logs -f sparkrouter
```

Open http://127.0.0.1:14673/ for the dashboard. The image has no shell, so `exec` is not available; use logs and the health endpoints. For remote access, put an HTTPS reverse proxy in front and set a dashboard password, see [Security and deployment](#security-and-deployment).

To upgrade, pull the new tag, then remove and recreate the container with the **same volume and env file** (for Quadlet: edit `Image=`, then `systemctl --user daemon-reload && systemctl --user restart sparkrouter`). Migrations run automatically at startup. Back up the volume first; stop the container for a consistent file copy:

```sh
docker stop sparkrouter
docker run --rm -v sparkrouter-data:/data:ro -v "$PWD":/backup docker.io/library/alpine:3 \
  tar czf /backup/sparkrouter-data.tgz -C /data .
docker start sparkrouter
```

(Substitute `podman` for `docker` as needed.)

#### Building the image yourself

```sh
docker build -t sparkrouter .     # or: podman build -t sparkrouter .
```

The build uses fully qualified base images, so Podman needs no short-name configuration. See [BUILD.md](BUILD.md#docker) for build details.

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

`/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/systemone`, `/v1/models`, and the [media endpoints](MEDIA.md#implemented-endpoints) are implemented as provider APIs. Requests route via `combo/<name>` or via an upstream model ID from a connection's last refresh. Combos support Priority, Round robin, and Random starting-step selection with ordered fallback. Direct IDs try matching enabled connections from oldest to newest, once each. Optional per-combo session/prompt affinity and timed Sticky placement do not cache responses. No Gemini, OAuth, adaptive routing optimization, price optimization, gateway queues, response caching, telemetry platform, or background model discovery. Model IDs are arbitrary strings. Steps can be edited inline without reordering. Model listing includes configured combos even if empty or disabled, plus distinct model IDs from enabled connections, filtered by key permissions.

Anthropic is a translation subset, not full OpenAI feature parity. It supports system/user/assistant/tool messages, text and images, function tools, and text/tool streaming. Multiple choices, developer/named messages, legacy functions, strict schemas, structured response formats, log probabilities, nonzero penalties, and other untranslatable generation options are rejected. Bookkeeping fields and image-detail hints are omitted; unknown extension fields pass through. Thinking, server-tool, and other auxiliary content blocks (and citation metadata) are not exposed as answer text. OpenAI temperatures above 1 are clamped to Anthropic's maximum of 1. `max_tokens` defaults to 8192 if unspecified. Set `max_tokens` or `max_completion_tokens` when a model rejects that ceiling or a different budget is required. Provider connection testing normally requires a `/models` endpoint; fal.ai has a documented starter catalog but no connectivity probe. Streams idle for 120 seconds end as incomplete responses. Master-key rotation is an offline command; the running server never changes its key.
