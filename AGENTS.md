# SparkRouter contributor and agent guide

## Mission and scope

SparkRouter is a deliberately small, self-hosted LLM gateway: one Rust package,
one executable, SQLite, and a server-rendered dashboard. A combo is an ordered
list of provider/model targets with Priority (default), Round robin, or Random
selection of the starting step; retryable failures use ordered fallback.

Implement the requested change, not OmniRoute's wider feature set. Do not add a
frontend service, Node.js runtime, SPA framework, external database, queues,
caches, background workers, or routing optimization without an explicit request.

Read these documents before substantial changes:

- [README.md](README.md): user setup, API behavior, and MVP boundaries.
- [BUILD.md](BUILD.md): prerequisites, building, and deployment.
- [ARCHITECURE.md](ARCHITECURE.md): module boundaries and runtime invariants.
- [TESTING.md](TESTING.md): validation commands and regression coverage.

## Working in this repository

1. Inspect existing files and relevant tests before editing. Preserve unrelated
   changes and live database files; never reset or delete user work.
2. Keep changes localized and consistent with nearby code. Prefer straightforward
   Rust and explicit control flow over generic frameworks or speculative traits.
3. Add regression tests for changed behavior. Use local mocks rather than real
   provider credentials or billable upstream requests.
4. If delegating bounded work, assign non-overlapping files and exact interfaces.
   Inspect the resulting changes and run tests before accepting them. Keep
   security decisions, streaming semantics, and integration judgment with the
   primary agent.
5. Update documentation when environment variables, endpoints, protocol support,
   security behavior, or deployment assumptions change.
6. Report what was verified and what could not be tested. Do not claim a Docker
   build or real-provider test succeeded unless it was actually run.

## Ownership and structure

| Location | Responsibility |
| --- | --- |
| `src/main.rs`, `src/config.rs`, `src/app.rs` | Startup, configuration, shared state, middleware, shutdown |
| `src/api/` | OpenAI-facing endpoints and boundary validation |
| `src/routing/combo.rs`, `src/routing/direct.rs` | Ordered attempts and retry policy |
| `src/providers/` | Protocol adapters, upstream HTTP, SSE framing |
| `src/db/`, `migrations/` | Persistence, constraints, transactional ordering |
| `src/crypto.rs`, `src/error.rs` | Credential protection and sanitized errors |
| `src/web/`, `templates/`, `static/` | Askama pages and small HTMX interactions |
| `tests/` | End-to-end tests with local upstream mocks |

The display name lives in `src/lib.rs` as `APP_NAME`. Vendors sharing a protocol
are configuration, not separate implementations. Model IDs are arbitrary strings.

## Non-negotiable invariants

- Preserve unknown OpenAI request fields. Route using `serde_json::Value`; change
  the target model without reconstructing the complete request schema.
- Attempt enabled combo steps sequentially from the strategy-selected start,
  at most once per request. Round robin advances once per request, not per retry.
  Permanent client/upstream request
  errors stop the chain; retry classification belongs in the routing helper.
  Target-specific 401, 402, 404, and 529 are retryable. 400, 403, and 422 are not.
  `combo/<name>` is always a combo. Direct public IDs are
  `<provider-slug>/<upstream-model-id>`; strip only the first prefix and preserve
  the complete upstream suffix. Try enabled connections within that provider
  whose last refresh listed it, oldest first, with once-each fallback. Never
  cross provider slugs or treat raw IDs as aliases. Combo steps retain upstream
  IDs unchanged. A missing model list never blocks a combo step. Each combo has
  one enforced category (Text, Decision, Image, Video, Music, TTS, STT);
  wrong-mode requests are rejected rather than filtering a mixed chain. Model
  IDs are never guessed into categories: adding an unclassified connection/model
  explicitly declares its category, constrained by known modalities and protocol
  support.
- **Never fall back after a streaming response has been returned to Axum.**
  Before commitment, adapters may report a retryable failure. After commitment,
  interruptions terminate the body; do not synthesize a successful ending.
- Reuse the shared `reqwest::Client`. Keep redirects disabled so credentials
  cannot be forwarded to an unexpected destination. Bound bodies and SSE events.
- Store configured API keys only as authenticated ciphertext. Keep the master
  key outside SQLite. Preserve exact ciphertext on a blank provider-key edit.
  An empty credential blob denotes an unauthenticated local provider, not a
  decryption failure that should be ignored.
- Never log keys, bearer headers, decrypted secrets, raw upstream error bodies,
  or unsanitized HTTP errors containing sensitive URLs. Do not add `Debug` to
  structures holding unprotected credential strings.
- Keep the default bind loopback. Gateway authentication protects `/v1/*`, not
  the dashboard. `ROUTER_API_KEY` is unrestricted; generated keys enforce exact
  public model-ID scopes and expiry before routing (including test chat) and
  filter `/v1/models`. Empty selected scopes deny all. Expired keys keep `/v1/*`
  closed until revoked; it stays open only when neither key type is configured.
  Health/readiness probes are public, sanitized, and never call providers.
  Preserve dashboard CSRF checks, origin checks, autoescaping,
  and security headers; they are not a substitute for dashboard authentication.
- Use parameterized SQL. Serialize step mutations in transactions and preserve
  unique positions, cascade combo deletion, and restricted provider deletion.
- For databases already deployed, add a new migration rather than editing an
  applied one. SQLx validates migration checksums.
- Embed frontend assets and migrations. No build-time database or frontend
  toolchain should be required. Keep `Cargo.lock` with the application.

## Validation before handoff

Run from the repository root:

```sh
cargo fmt
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

For packaging or embedded-asset changes, also run `cargo build --release --locked`
and smoke-test the resulting binary. Use a disposable database and a saved test
master key, never a user's production database. See [TESTING.md](TESTING.md) for
focused commands and manual acceptance checks.
