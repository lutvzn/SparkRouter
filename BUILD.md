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

## Configure and run

| Variable | Default | Purpose |
| --- | --- | --- |
| `ROUTER_MASTER_KEY` | Required | Base64 encoding of exactly 32 random bytes |
| `ROUTER_DATABASE_URL` | `sqlite://router.db` | SQLite storage location |
| `ROUTER_API_KEY` | Unset | Optional bearer key for `/v1/*`; empty is invalid |
| `ROUTER_BIND` | `127.0.0.1:8080` | IP address and port, not a hostname |
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

Open http://127.0.0.1:8080/providers. For development, `cargo run --locked` uses
the same environment configuration. Stop with Ctrl+C; Unix deployments also
handle SIGTERM for graceful shutdown.

**Do not generate a different master key every restart.** There is no automatic
rotation: an old database requires its original key to decrypt provider keys.
Back up that key separately from the database.

## SQLite and deployment layout

The database file is created and pending embedded migrations run at startup.
Its parent directory must already exist and be writable. Relative database paths
resolve from the process working directory, not the executable's location.

A typical installation has an executable, persistent database directory, and
environment configuration. SQLite may create `router.db-wal` and `router.db-shm`
while running. Do not delete these files from a live installation. Stop the
application before a simple file-copy backup, or use SQLite's online backup API.

The dashboard has no login. Leave the default loopback bind or use an
authenticated reverse proxy with TLS. `ROUTER_API_KEY` alone does **not** protect
provider management pages. See [ARCHITECURE.md](ARCHITECURE.md) for the trust model.

## Docker

The Dockerfile builds with `rust:1.94-bookworm`, then copies the release binary
into `debian:bookworm-slim` with CA certificates. It runs as UID **10001**.

After exporting your persistent master key and optional gateway key:

```sh
docker build -t sparkrouter .
docker volume create sparkrouter-data
docker run --rm --name sparkrouter \
  -p 127.0.0.1:8080:8080 \
  -v sparkrouter-data:/data \
  -e ROUTER_MASTER_KEY -e ROUTER_API_KEY \
  sparkrouter
```

Inside the container, defaults are `ROUTER_BIND=0.0.0.0:8080` and
`ROUTER_DATABASE_URL=sqlite:///data/router.db`. The example publishes only to
host localhost. Named-volume data survives container removal. Bind-mounted
directories must be writable by UID 10001. Docker supports `--env-file`, but
replace example placeholders and omit `ROUTER_BIND` if you want the container's
default network binding rather than `.env.example`'s loopback value.

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

For validation commands and acceptance checks, see [TESTING.md](TESTING.md).
