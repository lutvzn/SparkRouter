#!/usr/bin/env python3
"""Optional helper smoke test (Python 3 stdlib only); uses a disposable database.

Build the release executable first: cargo build --release --locked
Run: python scripts/tests/smoke.py
Windows uses PowerShell; Linux/macOS uses Bash. No upstream calls are made.
"""
import base64
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
WINDOWS = os.name == "nt"
BINARY = ROOT / "target" / "release" / ("sparkrouter.exe" if WINDOWS else "sparkrouter")
if not BINARY.is_file():
    raise SystemExit("Build first: cargo build --release --locked")
if WINDOWS:
    shell = shutil.which("pwsh") or shutil.which("powershell")
    if not shell:
        raise SystemExit("PowerShell is required")
    prefix = [shell, "-NoProfile", "-File", str(ROOT / "scripts" / "sparkrouter.ps1")]
    state_flag, env_flag = "-StateDir", "-EnvFile"
else:
    prefix = ["bash", str(ROOT / "scripts" / "sparkrouter.sh")]
    state_flag, env_flag = "--state-dir", "--env-file"

with tempfile.TemporaryDirectory(prefix="sparkrouter-helper-test-") as directory:
    temp = Path(directory)
    state = temp / "state"
    env_file = temp / "test.env"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    master = base64.b64encode(os.urandom(32)).decode()
    good_config = (
        f'ROUTER_MASTER_KEY="{master}"\n'
        f"ROUTER_DATABASE_URL=sqlite://{(temp / 'test.db').as_posix()}\n"
        f"ROUTER_BIND=127.0.0.1:{port}\nRUST_LOG=info\n"
    )
    env_file.write_text(good_config, encoding="utf-8")
    environment = {k: v for k, v in os.environ.items() if not k.startswith("ROUTER_")}

    def run(command, *arguments, expected=0):
        # Detached Windows children can inherit unused pipe handles. Capture to a
        # file so communicate() cannot wait forever for the child's pipe EOF.
        with (temp / "helper-output.txt").open("w+", encoding="utf-8", errors="replace") as output:
            result = subprocess.run(
                prefix + [command, state_flag, str(state), *arguments],
                cwd=temp,
                env=environment,
                stdout=output,
                stderr=subprocess.STDOUT,
                timeout=60,
                check=False,
            )
            output.seek(0)
            text = output.read()
        if result.returncode != expected:
            raise AssertionError(f"{command} returned {result.returncode}, expected {expected}:\n{text}")
        return text

    process_file = state / ("process.json" if WINDOWS else "process")

    def managed_id():
        content = process_file.read_text(encoding="utf-8-sig")
        return json.loads(content)["pid"] if WINDOWS else int(content.splitlines()[0])

    original_state = None
    try:
        generated = run("keygen").splitlines()[0].strip()
        assert len(base64.b64decode(generated, validate=True)) == 32
        run("status", expected=1)
        run("start", env_flag, str(env_file))
        run("status")
        first_id = managed_id()
        run("start", env_flag, str(env_file))
        assert managed_id() == first_id, "start must be idempotent"
        run("health", env_flag, str(env_file))
        run("logs")
        original_state = process_file.read_bytes()
        if WINDOWS:
            bad_state = json.loads(original_state.decode("utf-8-sig"))
            bad_state["startedUtcTicks"] = "1"
            process_file.write_text(json.dumps(bad_state), encoding="utf-8")
        else:
            lines = original_state.decode().splitlines()
            lines[1] = "wrong start time"
            process_file.write_text("\n".join(lines) + "\n", encoding="utf-8")
        run("stop")
        run("status", expected=1)
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/readyz", timeout=5) as response:
            assert response.status == 200, "identity mismatch must not stop the server"
        process_file.write_bytes(original_state)
        original_state = None

        lock = state / "lock"
        if WINDOWS:
            lock.touch()
        else:
            lock.mkdir()
        try:
            run("restart", env_flag, str(env_file), expected=1)
        finally:
            lock.unlink() if WINDOWS else lock.rmdir()
        run("restart", env_flag, str(env_file))
        run("health", env_flag, str(env_file))
        run("stop")
        run("stop")
        run("status", expected=1)

        env_file.write_text("ROUTER_API_KEY=\n", encoding="utf-8")
        run("help", env_flag, str(env_file), expected=1)
        env_file.write_text("PATH=unexpected\n", encoding="utf-8")
        run("help", env_flag, str(env_file), expected=1)
        marker = temp / "must-not-exist"
        expression = f"$(New-Item '{marker}')" if WINDOWS else f"$(touch '{marker}')"
        env_file.write_text(f'RUST_LOG="{expression}"\n', encoding="utf-8")
        run("help", env_flag, str(env_file))
        assert not marker.exists(), "environment files must not execute substitutions"
        env_file.write_text("ROUTER_MASTER_KEY=invalid\nROUTER_BIND=127.0.0.1:0\n", encoding="utf-8")
        run("start", env_flag, str(env_file), expected=1)
        assert not (state / "lock").exists(), "failed startup must release its lock"
        print("Helper smoke passed: lifecycle, health, identity, locking, and safe environment parsing.")
    finally:
        if original_state is not None:
            process_file.write_bytes(original_state)
        if process_file.exists():
            run("stop")
