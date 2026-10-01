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
| `src/db/` tests | Migrations, foreign keys, WAL persistence, CRUD, validation, ordering, transactional swaps/removals, delete constraints, timestamps |
| `src/routing/combo.rs` tests | Retry classification, per-combo/concurrent round-robin turns, unbiased random index selection |
| `src/providers/` tests | URL normalization, unknown-field preservation, model replacement, response translation, fragmented UTF-8/CRLF SSE, tools, usage, truncation, limits, stream commitment |
| `src/web/` tests | Exact blank-key preservation, credential replacement/no-key creation, form validation, escaping, CSRF fields/comparison, fragments and disabled labels |
| `tests/dashboard.rs` | Provider/combo CRUD, preset tiles and grouped named connections, explicit connection selection, HTMX ordering versus normal redirects, no key leakage, and no-auth provider routing |
| `tests/gateway.rs` | Mocked combo and direct-model fallback, permanent errors stopping the chain, disabled steps, models, optional API authentication, JSON passthrough, SSE behavior, Anthropic translation, provider edits, referrer policy, same-origin creation on a nondefault port, CSRF/origin rejection, restart persistence |
| `tests/redirects.rs` | Neither protocol follows redirects or forwards credentials to a redirected server |
| `tests/request_errors.rs` | Sanitized OpenAI-shaped errors for invalid JSON, wrong content type, and oversized bodies |

Library tests sit next to their implementation. Integration tests use the exported
library and `tower::ServiceExt::oneshot` to call the assembled Axum router. They
still use real HTTP for upstream requests, exercising Reqwest and protocol wiring.

## Focused commands

```sh
# One integration test file:
cargo test --locked --test gateway
cargo test --locked --test dashboard

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
4. Create `coding`, add two provider/model steps, reorder them, and remove/re-add
   a step. Verify the order persists after restarting with the **same** master key.
   Check full-width combo rows: name and Copy on the left; Duplicate, Edit, and
   Delete on the right. Copy must write `combo/coding`. Duplicate twice and check
   unique copy names, identical ordered targets, and independent edits/deletion.
   Check narrow layout and clipboard failure feedback without inline scripts.
   Combo titles should be near-white in dark mode and dark in light mode, with
   no underline on hover. Check local SVG icons on navigation and action buttons;
   Copy feedback and theme toggling must preserve their icons and text labels.
5. List virtual models and make normal and streaming calls:

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
   both API endpoints. Generate an API key and confirm it can call every combo, is shown only once, and is absent from the list and stored plaintext. Revoking the last generated key reopens the API when ROUTER_API_KEY is unset. Dashboard pages remain unauthenticated by design.
7. Verify ordered fallback using a controlled local mock that returns `503` for
   its first step and success for its second. A `400` must stop the chain. Do not
   deliberately generate rate-limit traffic against real providers.
   Priority must retain this behavior. Round robin should rotate enabled starting
   steps across requests (not across retries), skip disabled connections, and
   restart its sequence after process restart. Random should choose one enabled
   start per request; repeat selections are valid. For both strategies, verify
   cyclic fallback, permanent-error stopping, and no post-commit streaming retry.
8. Check that deleting a referenced provider is rejected. Delete its combo steps
   first, then retry. Check HTMX actions and their error display in a browser.
   Create providers and combos using their ordinary form submits too; the pages
   must send `Referrer-Policy: same-origin` so native POSTs retain a valid Origin.
9. Stop the process before inspecting SQLite files. Verify credential blobs are
   not plaintext; do not paste ciphertext, plaintext keys, or the master key into
   shared logs. Automated crypto tests provide stronger tamper/round-trip checks
   than simply searching file bytes.

## Packaging checks and limits of verification

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
