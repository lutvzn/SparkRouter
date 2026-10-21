#!/usr/bin/env bash
# Linux/macOS/WSL helper. Use sparkrouter.ps1 for native Windows processes.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ACTION=${1:-help}
[[ $# -eq 0 ]] || shift
STATE_DIR="$ROOT/.sparkrouter"
ENV_FILE=''
ENV_FILE_SPECIFIED=false
PROFILE=release
URL=''
TIMEOUT=30
usage() {
  printf '%s\n' 'Usage: bash scripts/sparkrouter.sh COMMAND [options]' \
    'Commands: start, run, stop, restart, rebuild, status, logs, health, build, check, test, keygen, help' \
    'Options: --env-file FILE  --state-dir DIR  --debug  --url URL  --timeout SECONDS' \
    'Defaults: release build, .sparkrouter state directory, 30-second stop timeout.' \
    'Loads .env from the caller working directory if present unless --env-file is specified.' \
    'Only plain KEY=value or quoted literal values are supported.' \
    'start builds if the binary is missing. run stays in the foreground (Ctrl+C to stop).' \
    'rebuild stops, builds, and starts again; a failed build leaves the server stopped.' \
    'stop sends SIGTERM; it never escalates to SIGKILL. Windows: use the PowerShell helper.'
}
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
while [[ $# -gt 0 ]]; do
  case "$1" in
    --env-file|--state-dir|--url|--timeout)
      [[ $# -ge 2 ]] || fail "Missing value for $1"
      case "$1" in
        --env-file) ENV_FILE=$2; ENV_FILE_SPECIFIED=true;; --state-dir) STATE_DIR=$2;; --url) URL=$2;; --timeout) TIMEOUT=$2;;
      esac
      shift 2;;
    --debug) PROFILE=debug; shift;;
    --help|-h) usage; exit 0;;
    *) fail "Unknown option: $1";;
  esac
done
case "$ACTION" in start|run|stop|restart|rebuild|status|logs|health|build|check|test|keygen|help) ;; *) fail "Unknown command: $ACTION";; esac
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ ]] || fail 'Timeout must be a positive integer'
# Resolve user paths before switching to the repository working directory.
[[ "$STATE_DIR" = /* ]] || STATE_DIR="$PWD/$STATE_DIR"
if [[ "$ENV_FILE_SPECIFIED" == false && -f .env ]]; then ENV_FILE='.env'; fi
if [[ -n "$ENV_FILE" ]]; then
  [[ -f "$ENV_FILE" ]] || fail 'Environment file not found'
  # Do NOT source/eval a file containing secrets or execute substitutions.
  while IFS= read -r line || [[ -n "$line" ]]; do
    line=${line%$'\r'}
    [[ "$line" =~ ^[[:space:]]*$ || "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || fail 'Invalid environment file line (expected KEY=value)'
    key=${BASH_REMATCH[1]}; value=${BASH_REMATCH[2]}
    case "$key" in ROUTER_MASTER_KEY|ROUTER_DATABASE_URL|ROUTER_API_KEY|ROUTER_BIND|ROUTER_DASHBOARD_PASSWORD|ROUTER_DASHBOARD_SECURE_COOKIE|RUST_LOG) ;; *) fail 'Unsupported environment variable in file';; esac
    if [[ "$value" == \"* || "$value" == \'* ]]; then
      quote=${value:0:1}
      [[ ${#value} -ge 2 && "${value: -1}" == "$quote" ]] || fail 'Unmatched environment value quote'
      value=${value:1:${#value}-2}
    fi
    [[ -n "$value" ]] || fail 'Invalid environment file: empty values are not supported; omit unset variables'
    export "$key=$value"
  done < "$ENV_FILE"
fi
cd -- "$ROOT"
BINARY="$ROOT/target/$PROFILE/sparkrouter"
PID_FILE="$STATE_DIR/process"
OUT="$STATE_DIR/stdout.log"
ERR="$STATE_DIR/stderr.log"
LOCK=''
unlock() { [[ -z "$LOCK" ]] || rmdir -- "$LOCK"; }
trap unlock EXIT
lock() {
  umask 077
  mkdir -p -- "$STATE_DIR"
  LOCK="$STATE_DIR/lock"
  if ! mkdir -- "$LOCK" 2>/dev/null; then
    LOCK=''; fail 'Another helper is running, or an abandoned lock exists; see BUILD.md'
  fi
}
# Match both command path and process start timestamp; never trust a PID alone.
identity() {
  local actual stamp
  [[ -f "$PID_FILE" ]] || return 1
  SAVED_ADDRESS=''
  { IFS= read -r PID; IFS= read -r SAVED_STAMP; IFS= read -r SAVED_BINARY; IFS= read -r SAVED_ADDRESS || true; } < "$PID_FILE" || fail 'Invalid process state; refusing to signal a PID'
  [[ "$PID" =~ ^[1-9][0-9]*$ && -n "$SAVED_STAMP" && -n "$SAVED_BINARY" ]] || fail 'Invalid process state; refusing to signal a PID'
  kill -0 "$PID" 2>/dev/null || return 1
  actual=$(ps -p "$PID" -o args= 2>/dev/null) || return 1
  stamp=$(ps -p "$PID" -o lstart= 2>/dev/null) || return 1
  [[ "$actual" == "$SAVED_BINARY" && "$stamp" == "$SAVED_STAMP" ]] || return 1
}
# Map wildcard binds to the matching loopback so the URL is usable locally.
local_url() {
  local bind=${ROUTER_BIND:-127.0.0.1:14673}
  case "$bind" in 0.0.0.0:*) bind="127.0.0.1:${bind#0.0.0.0:}";; '[::]:'*) bind="[::1]:${bind#\[::\]:}";; esac
  printf 'http://%s' "$bind"
}
address() {
  local bind=${ROUTER_BIND:-127.0.0.1:14673} url
  url=$(local_url)
  if [[ "$url" == "http://$bind" ]]; then printf '%s' "$url"; else printf '%s (bound to %s)' "$url" "$bind"; fi
}
build() {
  if [[ "$PROFILE" == release ]]; then cargo build --release --locked; else cargo build --locked; fi
}
ensure_binary() { [[ -x "$BINARY" ]] || build; }
start() {
  if identity; then printf 'Already running (PID %s)%s.\n' "$PID" "${SAVED_ADDRESS:+ at $SAVED_ADDRESS}"; return; fi
  [[ -n "${ROUTER_MASTER_KEY:-}" ]] || fail 'Set ROUTER_MASTER_KEY or pass --env-file FILE; use keygen once and save the key'
  ensure_binary
  : > "$OUT"; : > "$ERR"
  nohup "$BINARY" > "$OUT" 2> "$ERR" < /dev/null &
  PID=$!
  sleep 1
  if ! kill -0 "$PID" 2>/dev/null; then
    wait "$PID" || true
    fail "Startup failed; inspect $ERR (logs may contain configuration details)"
  fi
  SAVED_STAMP=$(ps -p "$PID" -o lstart=)
  SAVED_ADDRESS=$(address)
  printf '%s\n%s\n%s\n%s\n' "$PID" "$SAVED_STAMP" "$BINARY" "$SAVED_ADDRESS" > "$PID_FILE"
  printf 'Started PID %s at %s. Logs: %s\n' "$PID" "$SAVED_ADDRESS" "$STATE_DIR"
  printf 'Use health to check readiness; process startup alone does not prove readiness.\n'
}
stop() {
  if ! identity; then printf 'No matching managed process is running; no signal sent.\n'; return; fi
  kill -TERM "$PID"
  local waited=0
  while identity; do
    [[ $waited -lt $TIMEOUT ]] || fail 'Shutdown timed out; process state retained. Active streams may still be draining'
    sleep 1; waited=$((waited + 1))
  done
  rm -f -- "$PID_FILE"
  printf 'Stopped.\n'
}
case "$ACTION" in start|stop|restart|rebuild|run|status)
  case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) fail 'Use scripts/sparkrouter.ps1 for native Windows lifecycle management';; esac;;
esac
case "$ACTION" in
  help) usage;;
  keygen) command -v openssl >/dev/null || fail 'keygen requires OpenSSL'; openssl rand -base64 32; printf 'Save this master key securely and reuse it; this does not rotate stored credentials.\n' >&2;;
  build) build;;
  check) cargo fmt --check; cargo check --locked; cargo clippy --all-targets --all-features --locked -- -D warnings;;
  test) cargo test --locked;;
  start|stop|restart|rebuild)
    lock
    case "$ACTION" in start) start;; stop) stop;; restart) stop; start;; rebuild) stop; build; start;; esac;;
  run) [[ -n "${ROUTER_MASTER_KEY:-}" ]] || fail 'Set ROUTER_MASTER_KEY or pass --env-file FILE'; ensure_binary; printf 'Running at %s (Ctrl+C to stop).\n' "$(address)"; exec "$BINARY";;
  status) if identity; then printf 'Running (PID %s)%s.\n' "$PID" "${SAVED_ADDRESS:+ at $SAVED_ADDRESS}"; else printf 'Not running (or process identity no longer matches).\n'; exit 1; fi;;
  logs) for file in "$OUT" "$ERR"; do if [[ -f "$file" ]]; then printf '\n== %s ==\n' "$file"; tail -n 100 -- "$file"; fi; done;;
  health)
    command -v curl >/dev/null || fail 'health requires curl'
    [[ -n "$URL" ]] || URL=$(local_url)
    curl --fail --silent --show-error --max-time 5 "${URL%/}/readyz"; printf '\n';;
esac
