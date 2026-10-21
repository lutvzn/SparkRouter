# Building and running SparkRouter

## Prerequisites

- Rust **1.88 or newer**, Cargo, and the platform's linker/C toolchain. The Rust
  minimum is declared in `Cargo.toml`; use a current stable toolchain if unsure.
- `rustfmt` and Clippy for contributor checks:

  ```sh
  rustup component add rustfmt clippy
  ```

- Internet access for the initial Cargo dependency download, or a populated
  Cargo cache. Keep `Cargo.lock` and use `--locked` for reproducible dependency
  versions.

On Windows, install Visual Studio Build Tools with **Desktop development with
C++** and a Windows SDK for the MSVC Rust target. On Debian/Ubuntu, install
`build-essential` and `ca-certificates` alongside Rust.

SQLx builds bundled SQLite, so a C compiler is required. No SQLite server,
build-time `DATABASE_URL`, SQLx CLI, Node.js, npm, OpenSSL development headers,
or frontend build service is needed. Reqwest uses Rustls. OpenSSL's command-line
utility is only an optional way to generate a master key.

## Build

Run commands in the directory containing `Cargo.toml`:

```sh
cargo check --locked
cargo build --locked
cargo build --release --locked
```

Outputs:

| Platform | Release executable |
| --- | --- |
| Linux/macOS | `target/release/sparkrouter` |
| Windows | `target/release/sparkrouter.exe` |

The release profile strips symbols, uses thin LTO, and sets one codegen unit.
Migrations, Askama templates, and static assets are compiled into the application;
rebuild after changing them. Static assets are embedded in debug builds too.
The deployed executable does not need the source directories alongside it.

## Build-time models.dev categories

When Cargo runs `build.rs`, it downloads public metadata from
`https://models.dev/api.json?type=all` and embeds a conservative, exact-ID category
index. The request has a 20-second timeout, a 16 MiB size limit, and no redirects
or provider credentials. Failure or invalid data uses the checked-in
`data/models-dev-categories.json` snapshot; builds do not require models.dev to
be available. The runtime executable never contacts this service. Model classification review
shows the embedded source and date: successful live-download time, or the bundled
snapshot date (2026-10-10), including when a download failed. Offline builds retain
the fixed snapshot date rather than embedding a changing build timestamp.

Cargo caches build-script output; an unchanged build does not fetch again. Force
a new import by changing `SPARKROUTER_MODELS_DEV_REFRESH`, for example:

```sh
SPARKROUTER_MODELS_DEV_REFRESH=2026-10-11 cargo build --release --locked
```

For reproducible/offline builds, set `SPARKROUTER_MODELS_DEV_OFFLINE=1` (any
present value disables the download). This selects the bundled snapshot rather
than a previously downloaded catalog. Cargo dependencies still need a populated
cache for `--offline`. On PowerShell, set `$env:SPARKROUTER_MODELS_DEV_OFFLINE = '1'`
before building. These are build-only variables, not server configuration or
helper `.env` entries. Live-feed builds can classify newly recognized models
differently; review existing combo declarations after rebuilding. Only category
evidence is imported, not pricing, limits, providers, connections, or model lists.
See [category policy](README.md#category-specific-combos) for conservative mapping.

## Configure and run

| Variable | Default | Purpose |
| --- | --- | --- |
| `ROUTER_MASTER_KEY` | Required | Base64 encoding of exactly 32 random bytes |
| `ROUTER_DATABASE_URL` | `sqlite://router.db` | SQLite storage location |
| `ROUTER_API_KEY` | Unset | Optional bearer key for `/v1/*`; empty is invalid. Generated dashboard keys also authorize `/v1/*` |
| `ROUTER_BIND` | `127.0.0.1:14673` | IP address and port, not a hostname |
| `ROUTER_DASHBOARD_PASSWORD` | Unset | Optional single-admin login; nonblank 16–1024 bytes, preferably randomly generated |
| `ROUTER_DASHBOARD_SECURE_COOKIE` | `false` | Set `true` when serving through HTTPS; only `true`/`false` accepted |
| `RUST_LOG` | `info` | `tracing-subscriber` logging filter |

`.env.example` documents these values. **SparkRouter does not automatically load
`.env` files.** Supply variables through your shell, service manager, or container
configuration. Do not commit real credentials.

For a new installation, POSIX shell:

```sh
# Generate once; save securely and reuse for this database on future starts.
export ROUTER_MASTER_KEY="$(openssl rand -base64 32)"
export ROUTER_DATABASE_URL="sqlite://router.db"
export ROUTER_API_KEY="replace-with-a-long-random-gateway-key"
./target/release/sparkrouter
```

PowerShell 7, for a new installation:

```powershell
# Generate once; save securely and reuse for this database on future starts.
$bytes = [byte[]]::new(32)
[System.Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
$env:ROUTER_MASTER_KEY = [Convert]::ToBase64String($bytes)
$env:ROUTER_DATABASE_URL = 'sqlite://router.db'
$env:ROUTER_API_KEY = 'replace-with-a-long-random-gateway-key'
.\target\release\sparkrouter.exe
```

Open http://127.0.0.1:14673/providers. For development, `cargo run --locked` uses
the same environment configuration. Stop with Ctrl+C; Unix deployments also
handle SIGTERM for graceful shutdown.

**Do not generate a different master key every restart.** There is no automatic
rotation: an old database requires its original key to decrypt provider keys.
Back up that key separately from the database.

Request logging is configured in **Settings**, not `RUST_LOG`: Basic is
metadata-only (default), Off skips new history, and Extended requires confirmation
and stores bounded sensitive request/response bodies unencrypted in SQLite/WAL.
Use Extended only with test data and protected dashboard access. Body captures
are included in database backups, not configuration exports; clearing history
is not a secure wipe of WAL/free pages/backups. Mode and retention persist across
restart. See [request history](README.md#request-history-and-configuration-transfer).

Cache-aware/sticky routing is configured per combo, not through environment
variables. Off is the upgrade/default mode. Settings persist through SQLite
migrations, while bounded placement hints reset on restart and are not shared
between gateway processes. See [per-combo affinity](README.md#per-combo-cache-aware-and-sticky-routing).

Media uses the same executable and deployment environment; no frontend/media
worker or external queue is required. JSON bodies remain capped at 4 MiB, while
multipart media accepts up to 32 MiB including framing. Reverse proxies used for
media should permit those uploads and the gateway's preparation window (610
seconds for one target, growing per fallback target up to 30 minutes); binary output retains the 120-second upstream idle timeout. See
[MEDIA.md](MEDIA.md) for protocols and limits. No transcoder or media toolchain
is required.

## Convenience scripts

The repository includes matching command helpers:

- **Linux/macOS/WSL:** `bash scripts/sparkrouter.sh COMMAND [options]`
- **Windows:** `.\scripts\sparkrouter.ps1 COMMAND [options]` (Windows PowerShell
  5.1 or PowerShell 7). Git Bash users should use the PowerShell helper for native
  Windows lifecycle management.

If your execution policy blocks a downloaded script, review it and follow your
organization's policy; the helper does not change execution policy for you.

| Command | Behavior |
| --- | --- |
| `start` | Start in the background; build the selected binary if missing; repeated starts are idempotent |
| `run` | Run in the foreground, with Ctrl+C shutdown; not managed by `stop`/`status` |
| `stop` | Stop only the verified background process started by this helper |
| `restart` | Stop then start, using the supplied environment/settings |
| `rebuild` | Stop, build the selected profile, then start with the supplied settings; a failed build leaves it stopped |
| `status` | Report managed process identity; exit 1 if stopped or identity no longer matches |
| `logs` | Print the last 100 lines of both stdout and stderr logs |
| `health` | Call `/readyz`, with a five-second timeout; fails if not ready |
| `build` | `cargo build --release --locked` (or debug when requested) |
| `check` | Formatting check, `cargo check --locked`, and strict all-target/all-feature Clippy |
| `test` | `cargo test --locked` |
| `keygen` | Print a new random 32-byte Base64 master key; does not save or rotate keys |
| `help` | List commands/options |

`start` only verifies that the process survives initial startup; use `health` to
check readiness. An existing binary is reused; use `rebuild` after source changes
to stop, compile, and restart (or `build` followed by `restart`).
The scripts resolve their repository root and run the server there, regardless
of the caller's working directory. Thus the default SQLite URL still uses the
repository's `router.db`; use an explicit URL for another installation.

### Configuration and examples

The helpers automatically load `.env` from the caller's working directory if it
exists, before switching to the repository root. If absent, they use inherited
process environment variables. Specify `--env-file FILE` / `-EnvFile FILE` to load
another file instead; an explicitly requested missing file is an error.

```sh
# Generate ONCE for a new database; save the result securely.
bash scripts/sparkrouter.sh keygen
# Prepare .env using .env.example and the saved key. Do not overwrite an existing .env.
bash scripts/sparkrouter.sh build
bash scripts/sparkrouter.sh start
bash scripts/sparkrouter.sh status
bash scripts/sparkrouter.sh health
bash scripts/sparkrouter.sh logs
bash scripts/sparkrouter.sh restart
bash scripts/sparkrouter.sh rebuild
bash scripts/sparkrouter.sh stop
```

```powershell
.\scripts\sparkrouter.ps1 keygen
# Prepare .env using .env.example and the saved key. Do not overwrite an existing .env.
.\scripts\sparkrouter.ps1 build
.\scripts\sparkrouter.ps1 start
.\scripts\sparkrouter.ps1 status
.\scripts\sparkrouter.ps1 health
.\scripts\sparkrouter.ps1 logs
.\scripts\sparkrouter.ps1 restart
.\scripts\sparkrouter.ps1 rebuild
.\scripts\sparkrouter.ps1 stop
# For graceful Windows shutdown: run in the foreground and press Ctrl+C.
.\scripts\sparkrouter.ps1 run
```

Env files are UTF-8 (use no BOM for Bash), with `KEY=value` lines, optional matching
single/double quotes around literal values, blank lines, and full-line `#` comments.
They are **data, not executable shell code**: no `export`, variable expansion,
command substitution, multiline values, escape decoding, or inline comments.
Only the seven documented `ROUTER_*`/`RUST_LOG` variables from `.env.example` are
accepted. Empty values are rejected; omit optional unset variables instead.
File values override inherited environment values for the command. PowerShell
restores the caller's environment afterward. Errors do not echo submitted values.
Protect env files using restrictive filesystem permissions and never commit them.

Both helpers default to release builds. Use `--debug` / `-DebugBuild` for debug.
Bash `keygen` requires OpenSSL; Bash `health` requires curl. PowerShell uses built-in
cryptography and HTTP commands. Other lifecycle commands require only the selected
executable (and Cargo if it must be built).

### Process state and shutdown limits

PID/creation-time/executable identity and stdout/stderr logs live in the ignored
`.sparkrouter/` directory. Use `--state-dir DIR` / `-StateDir DIR` to choose another
location, and supply the same directory to every lifecycle command. A state
directory tracks one background instance. Env-file and state-directory relative
paths resolve from the caller's working directory. Logs are replaced on each
start and may contain configuration metadata; keep the directory private.

A lock serializes start/stop/restart/rebuild, including the entire rebuild.
`rebuild` starts the server even if it was already stopped. Pass the same env file
and state directory used for `start`; settings are not saved in the process state.
A stop failure aborts before building, and a build failure prevents startup. If a helper is forcibly killed,
an abandoned lock may remain: first confirm that no helper is running, then remove
only `.sparkrouter/lock` (a directory on Bash, a file on PowerShell). Never remove
state or lock files while a lifecycle operation is active. PID identity mismatch
causes status/stop to refuse ownership rather than terminate another process.
These checks protect against stale/reused PIDs; state files are not a security
boundary against someone with permission to edit your files or control processes.

On Unix, `stop` sends SIGTERM and waits up to 30 seconds for graceful shutdown.
Active streams may keep it draining; timeout leaves the process/state intact and
never escalates to SIGKILL. Adjust with `--timeout SECONDS`. On Windows, background
`stop` forcibly terminates the verified process and interrupts requests/streams;
`-TimeoutSeconds N` controls the wait for exit, not a graceful drain. Use foreground
`run` and Ctrl+C for graceful Windows shutdown. Forced shutdown does not provide a
clean database-backup window: use SQLite online backup or graceful shutdown.

`start`, `run`, and `status` print the address the server listens on, taken from
`ROUTER_BIND`; `status` reports the address recorded when `start` launched it.
`health` derives its URL from `ROUTER_BIND` (default loopback), mapping wildcard
binds to loopback. Override with `--url URL` / `-Url URL` for HTTPS/reverse proxies.
Pass the same env file when using a nondefault port. These helpers are conveniences,
not a service manager: no boot registration, recovery supervisor, log rotation,
backup automation, or master-key rotation is installed.

## SQLite and deployment layout

The database file is created and pending embedded migrations run at startup.
Its parent directory must already exist and be writable. Relative database paths
resolve from the process working directory, not the executable's location.

A typical installation has an executable, persistent database directory, and
environment configuration. SQLite may create `router.db-wal` and `router.db-shm`
while running. Do not delete these files from a live installation. Stop the
application before a simple file-copy backup, or use SQLite's online backup API.

The dashboard login is enabled only when `ROUTER_DASHBOARD_PASSWORD` is set.
Otherwise leave the default loopback bind or use an authenticated reverse proxy.
Use TLS for network access and set `ROUTER_DASHBOARD_SECURE_COOKIE=true` for HTTPS.
`ROUTER_API_KEY` alone does **not** protect provider management pages.
Sessions expire after eight hours and restart revokes them. Pass the dashboard
variables explicitly to Docker (`-e ROUTER_DASHBOARD_PASSWORD` and
`-e ROUTER_DASHBOARD_SECURE_COOKIE`) if desired.
See [ARCHITECURE.md](ARCHITECURE.md) for the trust model.

## Docker

The Dockerfile builds a fully static musl binary with `docker.io/library/rust:1.94-alpine` (the
build fails if the result is not statically linked), then copies it into
`gcr.io/distroless/static-debian13`. The runtime image has no shell, package
manager, or libc; Reqwest's TLS roots are embedded in the binary. It runs as
numeric UID/GID **10001**, so volumes from earlier images stay writable.
Because there is no shell, use `docker logs` and external probes rather than
`docker exec` for troubleshooting. For reproducible deployments, pin the base
images by digest in a derived Dockerfile.

After exporting your persistent master key and optional gateway key:

```sh
docker build -t sparkrouter .
docker volume create sparkrouter-data
docker run --rm --name sparkrouter \
  -p 127.0.0.1:14673:14673 \
  -v sparkrouter-data:/data \
  -e ROUTER_MASTER_KEY -e ROUTER_API_KEY \
  sparkrouter
```

Inside the container, defaults are `ROUTER_BIND=0.0.0.0:14673` and
`ROUTER_DATABASE_URL=sqlite:///data/router.db`. The example publishes only to
host localhost. Named-volume data survives container removal. Bind-mounted
directories must be writable by UID 10001. Docker supports `--env-file`, but
replace example placeholders and omit `ROUTER_BIND` if you want the container's
default network binding rather than `.env.example`'s loopback value.

### Podman

The same Dockerfile builds with Podman; base images are fully qualified, so no
short-name registry configuration is needed. Replace `docker` with `podman` in
the commands above. Named volumes need no extra steps. With rootless Podman,
container UID 10001 maps to a host subordinate UID, so a host bind-mounted
directory owned by your user is not writable by default. Either let Podman chown
it with `:U`, or chown it inside your user namespace once:

```sh
podman run --rm -p 127.0.0.1:14673:14673 \
  -v ./sparkrouter-data:/data:U,Z \
  -e ROUTER_MASTER_KEY -e ROUTER_API_KEY \
  sparkrouter
# or: podman unshare chown 10001:10001 ./sparkrouter-data
```

`Z` relabels the directory for SELinux hosts and is ignored elsewhere; do not use
it on shared system directories. The binary runs as PID 1 and handles SIGTERM,
so `podman stop` drains gracefully within its default 10-second timeout
(`podman stop -t 30` allows longer streams to finish).

## Troubleshooting

- **Linker/C compiler missing:** install the native toolchain described above.
- **Master-key configuration error:** check Base64 encoding and decoded length;
  do not substitute a password for 32 random bytes.
- **Encryption failure after restart:** restore the original master key. A new
  key cannot decrypt existing ciphertext.
- **SQLite cannot open or write:** check the URL, parent directory, process
  working directory, permissions, and container UID.
- **Address already in use:** choose another port through `ROUTER_BIND`.
- **Template/asset edits not visible:** rebuild and restart. Clear cached assets
  if necessary; HTMX's versioned file has a longer cache lifetime.

## Health checks

Use `GET /healthz` for liveness and `GET /readyz` for database readiness. Both
are public, independent of dashboard/API keys, support HEAD, and return small
JSON responses with `Cache-Control: no-store`. Readiness performs only `SELECT 1`
with a two-second deadline; failures return sanitized `503`, not redirects or
database diagnostics. Neither endpoint contacts providers or makes billable calls.
Readiness does not prove write access or upstream/model availability.

From the host or an external probe agent:

```sh
curl --fail http://127.0.0.1:14673/healthz
curl --fail http://127.0.0.1:14673/readyz
```

Configure your reverse proxy/orchestrator to use `/readyz`. The minimal Docker
runtime has no curl/wget and no automatic Docker HEALTHCHECK; provide an external
HTTP probe or a probe tool in your own derived image rather than relying on a
command that is absent from the image.

For validation commands and acceptance checks, see [TESTING.md](TESTING.md).
