# Testing SparkRouter

## Required checks

From the repository root, using the toolchain described in [BUILD.md](BUILD.md):

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Use `cargo fmt` to apply formatting. For executable, Docker, or embedded-asset
changes, also run:

```sh
cargo build --release --locked
```

Before the v1 release, historical-schema upgrade tests are not maintained.
Fresh database initialization still applies all embedded migrations, and the
suite tests current schema constraints and persistence. The migration files
remain unchanged.

These checks require **no real provider keys, running gateway, production
database, or configuration environment variables**. Fixtures construct
configuration directly with test-only keys. Tests create local mock HTTP servers
on `127.0.0.1:0` and temporary or in-memory SQLite databases. Loopback networking
and a writable temporary directory must be available.

The suite includes library and integration tests. List current tests with:

```sh
cargo test --locked -- --list
```

## Where coverage lives

| Location | Covered behavior |
| --- | --- |
| `src/crypto.rs` tests | Encryption round-trip, unique nonces, wrong key/provider context, tampering, invalid master-key lengths |
| `src/db/` tests | Fresh database initialization, foreign keys, WAL persistence, CRUD, validation, ordering, transactional swaps/removals, delete constraints, timestamps |
| `src/routing/` tests | Retry classification, per-combo/concurrent round-robin turns, unbiased random index selection, and pre-commit cancellation retaining attempt traces |
| `src/providers/` tests | URL normalization, unknown-field preservation, model replacement, response translation, fragmented UTF-8/CRLF SSE, tools, usage, truncation, limits, stream commitment |
| `src/web/` tests | Exact blank-key preservation, credential replacement/no-key creation, form validation, escaping, CSRF fields/comparison, fragments and disabled labels |
| `tests/provider_tags.rs`, `scripts/tests/provider-tags.cjs` | Text/Decision defaults, preset/custom multi-tag edits, empty overrides, validation/CSRF/origin, restart persistence, cascading deletion, combined name/any-tag filtering and empty state |
| `tests/dashboard.rs` | Provider/combo CRUD, preset tiles and grouped named connections, explicit connection selection, HTMX ordering versus normal redirects, no key leakage, and no-auth provider routing |
| `tests/test_chat.rs` | Test chat rendering, authenticated/CSRF-protected combo calls, actual fallback traces, sanitized failures, and shared routing strategy state |
| `tests/gateway.rs` | Mocked combo and direct-model fallback, permanent errors stopping the chain, disabled steps, models with exact cached context/output discovery (configured-first combo step, reorder, disabled/missing details, oldest enabled direct connection, scoped visibility), optional API authentication, JSON passthrough, SSE behavior, Anthropic translation, provider edits, referrer policy, same-origin creation on a nondefault port, CSRF/origin rejection, restart persistence |
| `tests/systemone.rs` | Dashboard evaluation results/traces and shared strategy state, CSRF/origin/auth/scope/validation, shared traffic admission, Native evaluation passthrough, model discovery/direct routing, endpoint protocol isolation, authenticated/scoped boundary, retryable versus permanent failures, invalid success fallback, custom-provider transfer |
| `tests/redirects.rs` | Neither protocol follows redirects or forwards credentials to a redirected server |
| `tests/request_errors.rs` | Sanitized OpenAI-shaped errors for invalid JSON, wrong content type, and oversized bodies |
| `tests/dashboard_auth.rs` | Optional admin login, cookie isolation/revocation, CSRF/origin protections, login limits |
| `tests/request_history.rs`, `tests/history_dashboard.rs` | Off/Basic/Extended, opt-in bounded body captures, JSON/SSE byte preservation, mode snapshot, partial/cancelled capture, escaping/no-store, metadata recording, fallback attempts, usage, streaming lifecycle, retention, escaping, protected clear |
| `tests/usage_dashboard.rs` | Overview summary/leaderboard, database connectivity/failure rendering, success/failure counts, memory parsing, cache and cost usage, streaming metrics, retained-window behavior and clearing statistics |
| `tests/settings.rs`, `scripts/tests/theme.cjs` | Settings navigation/sections, sanitized status, login/CSRF/origin, retention bounds and persistence, immediate/cascading pruning, confirmed clear, browser-local theme/system selection |
| `tests/config_transfer.rs` | Credential-free transfer, preview, validation, atomic rollback, fresh disabled connections |
| `tests/api_key_limits.rs`, `src/key_limits.rs` tests | Per-key quotas, atomic concurrent admission, exempt model listing/environment key, test-chat sharing, validation/CSRF, active policy edits, stream completion/error/drop and pre-commit cancellation |
| `tests/scoped_api_keys.rs` | Exact permissions, filtered model listing, expiry, closed gateway after expiry, policy edits/rollback, test-chat enforcement, restart persistence |
| `tests/combo_affinity.rs`, `src/routing/affinity.rs` tests | Per-combo Off/Basic/Smart/Sticky, header/body session IDs, exact prompt continuity/separation, caller isolation, clock-controlled expiry/capacity, fallback/rebinding, disabled/edited targets, successful/interrupted/dropped streams, dropdown validation/CSRF, duplication and backward-compatible transfer |
| `tests/combo_steps.rs` | In-place target edits, stable IDs/order/strategy, invalid targets, cross-combo isolation, CSRF/origin, fragment forms/escaping and local mock routing |
| `tests/health.rs` | Public liveness/readiness, HEAD, sanitized DB failure, bounded pool-acquisition timeout |
| `src/db/provider_slugs.rs` tests | Readable unique slugs, reserved names, transactional conflicts/concurrency, stable legacy initialization |

`tests/common/mod.rs` shares the counted local upstream server, RAII task cleanup,
and response readers used by the gateway, test-chat, and request-history suites.

Library tests sit next to their implementation. Integration tests use the exported
library and `tower::ServiceExt::oneshot` to call the assembled Axum router. They
still use real HTTP for upstream requests, exercising Reqwest and protocol wiring.

## Convenience-script checks

The helpers' `check` and `test` commands run the same Cargo checks documented above.
For helper changes, additionally run:

```sh
bash -n scripts/sparkrouter.sh
shellcheck scripts/sparkrouter.sh # Optional development tool, not a runtime requirement
cargo build --release --locked
python scripts/tests/smoke.py    # Optional Python 3 stdlib-only smoke harness
```

The smoke harness chooses PowerShell on Windows and Bash on Unix. It runs the
actual release executable against a temporary database with a saved temporary
master key; it never loads a user's `.env` or contacts providers. It covers key
generation, start/status/idempotency, readiness, logs, restart/stop,
rebuild from running/stopped states, failed-build shutdown and lock cleanup, process
identity mismatch, lock conflicts, failed startup, empty-key rejection, and
literal env-file parsing. A native release binary is required for the host OS.
On Windows it captures helper output to files, because detached children can
inherit unused pipe handles and delay captured-pipe EOF.

Manually verify foreground Ctrl+C shutdown on each target OS. Unix stop must not
escalate to SIGKILL on timeout; Windows background stop is explicitly forced.
MacOS and Windows PowerShell 5.1 compatibility require checks on those environments,
not just Bash syntax or PowerShell 7 execution.

For clipboard JavaScript changes, optionally run the dependency-free development
test (Node.js is not required to build or run SparkRouter):

```sh
node scripts/tests/clipboard.cjs
node scripts/tests/theme.cjs
node scripts/tests/test-chat.cjs
node scripts/tests/tooltips.cjs
node scripts/tests/provider-tags.cjs
```

For provider tags, manually check checkbox filters to the right of search on
wide screens and wrapping on phones, in both themes. Verify keyboard focus,
any-tag matching plus name search, no-selection reset, tag edits on preset and
custom pages, and ordinary Save tags submissions without JavaScript. DOM mocks
do not establish browser layout or accessibility.

The test-chat checks also cover SystemOne mode switching, JSON validation,
native evaluation payloads/results, safe text rendering, repeat/retry, and
pagehide cleanup. Manually check the SystemOne state/questions editors and
formatted result at phone and desktop widths; DOM mocks do not prove layout.

It covers modern clipboard copying, plain-HTTP fallback, failure feedback,
temporary textarea cleanup, focus restoration, and toast dismissal/reset. Also manually check Copy on
an HTTP LAN address and HTTPS/localhost in a browser; mocked DOM tests do not
prove browser clipboard permissions.

## Focused commands

```sh
# One integration test file:
cargo test --locked --test gateway
cargo test --locked --test dashboard
cargo test --locked --test test_chat
cargo test --locked --test combo_affinity
cargo test --locked --test systemone

# Library module filters:
cargo test --locked --lib crypto::tests
cargo test --locked --lib db::combos::tests
cargo test --locked --lib providers::anthropic::tests

# One regression by name:
cargo test --locked --test gateway mid_stream_upstream_failure_does_not_fall_back
cargo test --locked --test gateway provider_edit_with_blank_api_key_preserves_ciphertext

# Show captured output while investigating a failure:
cargo test --locked --test gateway -- --nocapture
```

Use `-- --test-threads=1` only to investigate suspected interactions; tests should
remain independent and pass under the default parallel runner. Rust test-name
arguments are substring filters, not file paths.

## Adding regression tests

- Choose unit tests for pure transformations/classification and integration tests
  for middleware, database, HTTP adapter, or dashboard wiring.
- Use fake credentials and local Axum mock servers. Never use a real user's
  database, bearer token, or paid upstream service in the automated suite.
- Bind mocks to port zero and shut tasks down with RAII/abort guards so tests do
  not leave listeners running.
- Coordinate streaming failures with channels, not sleeps. The mid-stream test
  releases the failure only after the client reads the first forwarded frame.
- Assert both the response and attempt counts: a valid result alone does not
  prove the right provider was selected or that a permanent error stopped fallback.
- For passthrough, assert nested unknown fields and the replacement model. For
  blank-key edits, assert exact ciphertext equality, not just successful decryption.
- Exercise split SSE events/UTF-8, missing completion markers, early versus late
  errors, and size limits when changing stream handling. Late failures must not
  contact a backup provider.
- For disk persistence, reopen using the same master key and close pools before
  deleting temporary files, especially on Windows.

Do not silently weaken assertions to accommodate a changed interface. Inspect
the intended behavior, update helpers, and keep precise protocol assertions.

## Manual acceptance smoke test

On a provider page, check that the name and subtitle appear in the top bar,
the Custom badge stays in the wide details card, and **Add connection** opens
a popup. Check focus, Close/Cancel, Escape, native form submission, and narrow
viewport layout in both themes. The popup uses native HTML popover controls,
so it also works with JavaScript disabled in browsers supporting popovers.

Use a **disposable database** and a saved test master key. Start the release
executable following [BUILD.md](BUILD.md); never point an exploratory test at an
existing installation's database.

1. Open `/providers`; verify navigation, empty states, local CSS, and HTMX load.
    Verify Providers/Combos links are in the left sidebar, not the top toolbar;
    check navigation and the compact layout on narrow screens.
    Open `/` and verify Overview is the default landing page and login
    destination. Check database status, successful/failed request counts, and process
    RAM (Linux only); reload to refresh. Pending streams must not count as finished,
    and unavailable history must not appear as zero. Check its five usage cards and top-ten provider ranking using
    disposable mock history; verify dark/light themes and mobile layout.
   Providers and Combos list titles/subtitles should appear in the top bar beside
   the theme toggle, without duplicate headings in the content area.
   With no saved preference, check that the theme and button match OS light/dark
   changes. Each click switches directly to the opposite mode; manual choices
   survive navigation/reload. Check form controls and narrow screens in both themes.
2. Create two OpenAI-compatible providers with appropriate base URLs and model
   access. Test saved connectivity; remember `/models` support does not guarantee
   generation access.
   Check the provider grid shows six columns on wide screens, never more than
   six, and fewer columns as the viewport narrows without horizontal overflow.
    Check compact card headers, protocol/Custom badges sharing the metadata row,
   and inline status badges in both themes. Card URLs are hidden; provider pages
   still display the full endpoint. Long names must not overflow the cards.
   Create a custom provider using only name, base URL, and protocol; its empty
   card must appear before any key is added. Open the card, add two named keys,
   and verify the card remains after both connections are deleted. Distinct
   named custom providers may share URLs without merging their connections.
   Check the Custom badge and Delete provider action. Deletion removes the
   provider and all its connections, but a connection used by a combo must block
   the entire deletion without losing any keys. Presets have no provider-delete action. The providers search box filters tile titles as you type, without Enter, and does not match the protocol badge alone. Refresh models on a connection, open the model dropdown, and confirm the search box filters cards as you type. Those IDs should also appear as combo-step suggestions; a typed ID must still be accepted.
   Check preset tiles and Custom provider. Add two named connections to one
   preset and verify both appear on its page without exposing keys. Connections
   in separate custom providers must remain separate. Confirm the combo
   add-step form chooses a provider, then a connection, and targets that key only.
3. Edit a provider with the key field blank; verify it still authenticates. Toggle
    enabled state and verify its label. Ensure keys never appear in page HTML.
   Create two custom providers with slugs `provider1` and `provider2`, both listing
   the same upstream model. Confirm `/v1/models` and Copy expose separate qualified
   IDs; each request reaches only its provider, with no cross-provider fallback
   on failure. Confirm combo suggestions still use raw upstream IDs. Check exact
   API-key scopes allow only the selected provider/model. Verify omitted slugs
   gain readable collision suffixes, reserved/duplicate explicit slugs are rejected,
   and restart/export/import preserve slugs (imports suffix conflicts).
4. Create `coding`, add two provider/model steps, reorder them, and remove/re-add
   a step. Verify the order persists after restarting with the **same** master key.
   Check full-width combo rows: name and Copy on the left; Duplicate, Edit, and
   Delete on the right. Copy must write `combo/coding`. Duplicate twice and check
   unique copy names, identical ordered targets, and independent edits/deletion.
   Check narrow layout and clipboard failure feedback without inline scripts.
   Combo titles should be near-white in dark mode and dark in light mode, with
   no underline on hover. Check local SVG icons on navigation and action buttons;
   Copy feedback and theme toggling must preserve their icons and text labels.
5. Open **Test** in the sidebar. Choose a combo, enter a gateway key if required,
   and send a message. Verify the assistant reply and ordered attempt details,
   including a controlled retryable failure and a permanent failure with local
   mocks. Repeated requests should follow the combo strategy. Check multi-turn
   history, reset, combo changes, errors, and narrow layouts in both themes.
   Check the centered empty state, right-aligned user bubbles, assistant replies,
   single-line bottom composer with Send on its right, New chat, and collapsed
   Routing attempts panel. Enter sends; modified Enter and IME composition must
   not submit. Failed requests retain the draft for retry.
   Refreshing or leaving the page should discard the chat and entered key.
   These are real upstream generations when using real connections.

   List virtual models and make normal and streaming calls:

   ```sh
   curl http://127.0.0.1:8080/v1/models \
     -H "Authorization: Bearer $ROUTER_API_KEY"

   curl http://127.0.0.1:8080/v1/chat/completions \
     -H "Authorization: Bearer $ROUTER_API_KEY" \
     -H 'Content-Type: application/json' \
     -d '{"model":"combo/coding","messages":[{"role":"user","content":"Hello"}]}'

   curl -N http://127.0.0.1:8080/v1/chat/completions \
     -H "Authorization: Bearer $ROUTER_API_KEY" \
     -H 'Content-Type: application/json' \
     -d '{"model":"combo/coding","stream":true,"messages":[{"role":"user","content":"Hello"}]}'
   ```

   Omit Authorization if ROUTER_API_KEY is unset and no dashboard API key exists. In PowerShell, use `curl.exe`
   or its native HTTP commands and `$env:ROUTER_API_KEY` for environment access.

6. If a gateway key is configured, verify missing/wrong keys receive `401` on
both API endpoints. Generate an unrestricted API key and confirm it can call every combo, is shown only once, and is absent from the list and stored plaintext. Revoking the last generated key reopens the API when ROUTER_API_KEY is unset. With no dashboard password, pages remain open. With `ROUTER_DASHBOARD_PASSWORD` set, check login, wrong password, logout/replay, restart revocation, and that gateway bearer keys do not grant dashboard access. Set Secure cookies only when testing through HTTPS.
   Create a selected-model key with an expiry. Check its filtered `/v1/models`,
   allowed JSON/streaming/test-chat calls and disallowed 403 responses without
   upstream attempts. Edit scopes/expiry without changing its secret. Expire a
   disposable test key: it must return 401, not reopen the gateway. Confirm
   environment and legacy unrestricted keys remain unrestricted.
   Set a generated key's requests/minute and concurrency limits using local
   mocks. Confirm over-limit chat/test-chat receives 429 with Retry-After and no
   upstream attempt, while model listing still works. Keep one mock stream open
   and confirm its concurrency slot remains held until completion/cancellation.
   Reload API keys to inspect active counts and current-window rejections. Blank
   both fields to remove limits; edits must not change the key secret or interrupt
   active requests. Restart: limits persist but counters reset.
7. Verify ordered fallback using a controlled local mock that returns `503` for
   its first step and success for its second. A `400` must stop the chain. Do not
   deliberately generate rate-limit traffic against real providers.
   Priority must retain this behavior. Round robin should rotate enabled starting
   steps across requests (not across retries), skip disabled connections, and
   restart its sequence after process restart. Random should choose one enabled
   start per request; repeat selections are valid. For both strategies, verify
   cyclic fallback, permanent-error stopping, and no post-commit streaming retry.
   On each combo's create/edit page, check the separate Off/Basic/Smart/Sticky
   dropdown (Off by default) and Sticky minutes (1–1440, default 5). Basic should
   retain a local mock target for the same session-id header; without an ID it
   follows the strategy. Smart should retain the target across growing messages
   with an unchanged initial prompt/tools, and separate changed first prompts.
   Sticky should retain the last successful target for that key/combo even when
   sessions change; after its idle timeout it follows the strategy again. Check
   successful requests renew the timeout, retries rebind, and permanent errors
   stop. Hints reset on restart/edits but mode/timeout persist. Check ordinary
   non-JS forms, all four options, saved minutes, narrow layouts, and both themes.
   Combo settings should use one full-width card with four compact columns on
   wide screens, two on tablets, and one on phones. Descriptions belong to the
   info buttons: verify hover, keyboard focus, tap-to-focus, Escape dismissal,
   pointer movement into the tooltip, and no form submission on info clicks.
   Test chat New chat/combo changes should generate a fresh page-local ID;
   Sticky deliberately ignores New chat until expiry.
8. Check that deleting a referenced provider is rejected. Delete its combo steps
   first, then retry. Check HTMX actions and their error display in a browser.
   Create providers and combos using their ordinary form submits too; the pages
   must send `Referrer-Policy: same-origin` so native POSTs retain a valid Origin.
9. Stop the process before inspecting SQLite files. Verify credential blobs are
   not plaintext; do not paste ciphertext, plaintext keys, or the master key into
   shared logs. Automated crypto tests provide stronger tamper/round-trip checks
   than simply searching file bytes.

## Packaging checks and limits of verification

For the new dashboard pages, also verify:

- In a disposable database, check request logging defaults to Basic. Off must
  stop new API and Test chat history/Overview increments, but not erase old rows.
  Extended must require the sensitive-content checkbox. Inspect JSON and SSE
  bodies via History, including large/truncated bodies and interrupted streams;
  content must be escaped, headers and Test chat CSRF excluded. Verify mode
  persistence/restart, clear/prune cascading, and dashboard authentication on
  inspection pages. Do not use production prompts or credentials; captures are
  unencrypted and may remain in WAL/backups after clear.
- Open Settings via the sidebar; verify its active state and five sections in
  light/dark themes and at mobile widths. Select System and check OS changes;
  preferences should survive reload in the same browser only. With blocked
  browser storage, check the page-only warning. Reduce retention in a disposable
  database and verify irreversible pruning, History's displayed cap, Overview
  totals, and persistence after restart. Confirm clearing requires the checkbox.
  Export/import and preview should stay within Settings and reuse credential-free
  transfer, with no separate Configuration sidebar entry or page. Check Refresh updates
  uptime and status; deployment secrets must not appear in HTML. Check ordinary
  retention/clear forms with JavaScript disabled.

- Make successful, fallback, and interrupted-stream requests using local mocks;
  inspect History outcomes, attempts, usage (unknown if omitted), pagination, and
  Clear. Confirm conversation text is absent, including after restarting.
- In `tests/usage_dashboard.rs` and the browser smoke test, cover cache-read and
  explicit USD cost extraction, stream usage, unknown versus explicit-zero values,
  retry selected-attempt-only accounting, provider ranking and retained-window
  clearing. Verify cache reads remain a subset of input and unknown/non-USD costs
  are not estimated; coverage counts must make partial totals apparent.
- Export configuration, preview it in a second disposable instance using a
  different master key, and confirm import. Check empty/custom provider groups,
  named connections, combo strategies and ordering; connections must be disabled
  and credential-free. Add keys and enable explicitly. Conflicting combo names
  or invalid references must leave the database unchanged.
- Check Login, History, Settings (including import preview), and Logout in both themes and on narrow
   screens, including ordinary forms with JavaScript disabled.
- Check one highlighted current navigation section on list and nested edit pages,
  a single page heading in the shared header, keyboard-visible focus rings and
  the Skip to content link. Check consistent text, number, password, select, and
  multiline controls. At 375px and desktop widths, only table regions may scroll
  horizontally; connection actions and navigation must wrap rather than overflow.
- Open model details for multiple models and connections, including after an
  HTMX refresh. Native popover targets must remain distinct. Check Close, Escape,
  focus return, and details with JavaScript disabled; no custom modal script is
  required. Check light, dark, and system themes without external font requests.
- Edit multiple combo steps in place, including a disabled connection and an
  arbitrary typed model. Check the Edit button is immediately left of Remove,
  toggles only its own editor, exposes expanded state, and works after swaps.
  Without JavaScript, the native Edit step disclosure should remain usable.
  Check stable order/IDs, provider filtering and per-form
  suggestions after HTMX swaps, ordinary non-JS submits, and both themes.
- With dashboard and API authentication enabled, `/healthz` and `/readyz` must
  still return public 200/no-store responses (HEAD too). Confirm a closed or
  exhausted disposable DB pool returns sanitized readiness 503 within two seconds
  without changing liveness. No upstream calls should occur.

- Run the release binary from a different working directory with an explicit
  database URL; dashboard assets and migrations should still be available without
  copying source folders.
- If Docker is available, build/run the documented image, exercise the API, and
  restart using the same volume and master key to confirm persistence. Check that
  a bind-mounted directory is writable by UID 10001.
- Browser smoke checks are separate from Rust tests: automated handler tests do
  not execute HTMX/JavaScript or verify visual layout and browser CSP behavior.
- Real-provider compatibility, network latency, load capacity, reverse-proxy
  buffering, and container builds are not established by local mock tests alone.
  Explicitly report which checks were run and any environment blockers.

For streaming ownership, timeout policy, and security boundaries, consult
[ARCHITECURE.md](ARCHITECURE.md).
