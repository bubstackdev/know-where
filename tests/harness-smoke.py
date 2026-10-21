#!/usr/bin/env python3
"""Exercise installed Claude/Codex tools through knowwhere and a fake OpenRouter.

No account is needed. Harness homes, configuration, credentials, project files,
and router state are temporary. External HTTP(S) is routed to a closed loopback
proxy; the only inference endpoint is the local fake server.

Run after `cargo build`:
  python3 tests/harness-smoke.py --require-installed
  python3 tests/harness-smoke.py --harness codex --binary target/debug/knowwhere
"""
from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

MODEL = "openai/knowwhere-harness-smoke"
FINAL = "KNOW_WHERE_HARNESS_OK"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def isolated_environment(home: Path) -> dict[str, str]:
    env = {key: os.environ[key] for key in (
        "PATH", "LANG", "LC_ALL", "TMPDIR", "TEMP", "TMP", "SHELL",
        "SYSTEMROOT", "COMSPEC", "PATHEXT",
    ) if key in os.environ}
    env.update({
        "HOME": str(home),
        "USERPROFILE": str(home),
        "CODEX_HOME": str(home / ".codex"),
        "CLAUDE_CONFIG_DIR": str(home / ".claude"),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DISABLE_ERROR_REPORTING": "1",
        "DISABLE_AUTOUPDATER": "1",
        "OTEL_SDK_DISABLED": "true",
        "NO_PROXY": "127.0.0.1,localhost,::1",
        "no_proxy": "127.0.0.1,localhost,::1",
        "HTTP_PROXY": "http://127.0.0.1:9",
        "HTTPS_PROXY": "http://127.0.0.1:9",
        "ALL_PROXY": "http://127.0.0.1:9",
        "http_proxy": "http://127.0.0.1:9",
        "https_proxy": "http://127.0.0.1:9",
        "all_proxy": "http://127.0.0.1:9",
        "NO_COLOR": "1",
        "TERM": "dumb",
    })
    return env


def run(command: list[str], *, cwd: Path, env: dict[str, str], timeout: int = 70) -> subprocess.CompletedProcess:
    process = subprocess.Popen(
        command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        start_new_session=os.name != "nt",
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        if os.name == "nt":
            process.terminate()
        else:
            os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                process.kill()
            else:
                os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
        raise AssertionError(f"Harness command timed out: {command[0:3]}\n{stdout[-4000:]}\n{stderr[-4000:]}") from None
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


class FakeOpenRouter:
    def __init__(self, harness: str, fixture: Path, marker: str):
        self.harness, self.fixture, self.marker = harness, fixture, marker
        self.requests: list[dict] = []
        self.errors: list[str] = []
        self.returned_tool = False
        self.saw_tool_output = False
        self.returned_final = False
        self.lock = threading.Lock()
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def json(self, value, status=200):
                body = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                if self.path.split("?")[0].endswith("/models"):
                    self.json({"data": [{
                        "id": MODEL, "name": "Local harness smoke fixture", "context_length": 128000,
                        "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                        "supported_parameters": ["tools", "tool_choice", "max_tokens"],
                        "pricing": {"prompt": "0", "completion": "0", "input_cache_read": "0"},
                    }]})
                elif self.path.split("?")[0].endswith("/key"):
                    self.json({"data": {"label": "Local fake key", "is_free_tier": False, "limit": None}})
                else:
                    self.json({"error": {"message": "Unknown fake-provider route"}}, 404)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
                if not self.path.split("?")[0].endswith("/chat/completions"):
                    self.json({"error": {"message": "Unexpected fake-provider POST route"}}, 404)
                    return
                with fake.lock:
                    fake.requests.append(body)
                    try:
                        chunks = fake.complete(body)
                    except AssertionError as error:
                        fake.errors.append(str(error))
                        self.json({"error": {"message": str(error)}}, 400)
                        return
                if body.get("stream"):
                    payload = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
                    data = payload.encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    self.wfile.flush()
                else:
                    delta = chunks[0]["choices"][0]["delta"]
                    for chunk in chunks[1:]:
                        addition = chunk["choices"][0]["delta"]
                        if addition.get("content"):
                            delta["content"] = delta.get("content", "") + addition["content"]
                        if addition.get("tool_calls"):
                            delta["tool_calls"][0]["function"]["arguments"] += addition["tool_calls"][0]["function"]["arguments"]
                    self.json({"id": "gen_smoke", "model": MODEL, "choices": [{"message": dict(delta, role="assistant"), "finish_reason": chunks[-1]["choices"][0]["finish_reason"]}], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "cost": 0}})

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}/api/v1"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def complete(self, body: dict) -> list[dict]:
        assert body.get("model") == MODEL, f"Unexpected upstream model: {body.get('model')}"
        assert body.get("provider", {}).get("max_price") == {"prompt": 1, "completion": 1}, "Saved price caps missing from harness request"
        tool_messages = [message for message in body.get("messages", []) if message.get("role") == "tool"]
        if tool_messages:
            assert self.returned_tool, "Tool output arrived before a model tool call"
            assert any(self.marker in json.dumps(message.get("content")) for message in tool_messages), f"Tool did not read the fixture: {tool_messages}"
            self.saw_tool_output = True
            self.returned_final = True
            return [self.chunk({"content": FINAL[:12]}), self.chunk({"content": FINAL[12:]}, "stop")]
        assert not self.returned_tool, "Harness repeated inference without returning its tool result"
        functions = [tool.get("function", tool) for tool in body.get("tools", [])]
        names = [function.get("name") for function in functions]
        expected = "Read" if self.harness == "claude" else "exec_command"
        assert expected in names, f"Expected {expected} tool, advertised tools: {names}"
        arguments = {"file_path": str(self.fixture)} if self.harness == "claude" else {
            "cmd": "cat " + shlex.quote(str(self.fixture)), "login": False, "max_output_tokens": 1000,
        }
        encoded = json.dumps(arguments)
        midpoint = len(encoded) // 2
        self.returned_tool = True
        return [
            self.chunk({"tool_calls": [{"index": 0, "id": "call_smoke", "type": "function", "function": {"name": expected, "arguments": encoded[:midpoint]}}]}),
            self.chunk({"tool_calls": [{"index": 0, "function": {"arguments": encoded[midpoint:]}}]}, "tool_calls"),
        ]

    @staticmethod
    def chunk(delta: dict, finish: str | None = None) -> dict:
        return {"id": "gen_smoke", "object": "chat.completion.chunk", "model": MODEL,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                **({"usage": {"prompt_tokens": 20, "completion_tokens": 5, "cost": 0}} if finish else {})}


def exercise(binary: Path, harness: str, executable: str, *, claude_normal: bool = False):
    with tempfile.TemporaryDirectory(prefix=f"knowwhere-{harness}-smoke-") as temporary:
        home = Path(temporary)
        work = home / "work"
        work.mkdir()
        for directory in (home / ".codex", home / ".claude"):
            directory.mkdir()
        config = home / ".codex" / "config.toml"
        config.write_text('model = "saved-model-untouched"\n')
        claude_settings = home / ".claude" / "settings.json"
        claude_settings.write_text('{"env":{"KNOW_WHERE_SAVED_SENTINEL":"untouched","ANTHROPIC_BASE_URL":"http://127.0.0.1:9","ANTHROPIC_API_KEY":"fake-saved-api-key","ANTHROPIC_AUTH_TOKEN":"fake-saved-auth-token"}}\n')
        codex_auth = home / ".codex" / "auth.json"
        codex_auth.write_text('{"OPENAI_API_KEY":"fake-saved-key-never-use"}\n')
        claude_auth = home / ".claude" / ".credentials.json"
        claude_auth.write_text(json.dumps({"claudeAiOauth": {
            "accessToken": "fake-saved-token-never-use", "refreshToken": "fake-saved-refresh-never-use",
            "expiresAt": int(time.time() + 86400) * 1000,
        }}) + "\n")
        supplied_settings = work / "session-settings.json"
        supplied_settings.write_text(json.dumps({"permissions": {"allow": ["Read"]}, "env": {
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:9", "ANTHROPIC_API_KEY": "fake-supplied-api-key",
            "ANTHROPIC_AUTH_TOKEN": "fake-supplied-auth-token", "KNOW_WHERE_USER_PREF": "preserved",
        }}) + "\n")
        before = {path: digest(path) for path in (config, claude_settings, codex_auth, claude_auth, supplied_settings)}
        marker = "KNOW_WHERE_READ_" + uuid.uuid4().hex
        fixture = work / "fixture.txt"
        fixture.write_text(marker + "\n")
        env = isolated_environment(home)
        # Resolve executables before HOME isolation; the launcher inherits only
        # this PATH, so it selects the same installed binary used for --version.
        env["PATH"] = str(Path(executable).parent) + os.pathsep + env.get("PATH", "")
        version = subprocess.run([executable, "--version"], env=env, cwd=work, capture_output=True, text=True, timeout=10)
        print(f"Testing {harness}: {version.stdout.strip()}")
        with FakeOpenRouter(harness, fixture, marker) as fake:
            common = [str(binary), "--data-dir", str(home / "router"), "--openrouter-api-base-url", fake.base]
            setup_env = dict(env, OPENROUTER_API_KEY="fake-only-never-a-live-key")
            setup = run(common + ["setup", "--model", MODEL], cwd=work, env=setup_env)
            assert setup.returncode == 0, f"Setup failed:\n{setup.stdout}\n{setup.stderr}"
            if harness == "claude":
                args = ([] if claude_normal else ["--bare"]) + ["--settings", str(supplied_settings), "-p", "--tools", "Read", "--allowedTools", "Read",
                        "--permission-mode", "dontAsk", "--permission-prompts", "none",
                        "--no-session-persistence", "--output-format", "stream-json", "--verbose",
                        "Read fixture.txt using Read, then report the result."]
            else:
                args = ["exec", "--ignore-user-config", "--ignore-rules", "--ephemeral",
                        "--skip-git-repo-check", "--json", "--sandbox", "read-only",
                        "Read fixture.txt using exec_command, then report the result."]
            result = run(common + ["run", harness, "--"] + args, cwd=work, env=env)
            assert result.returncode == 0, f"{harness} exited {result.returncode}:\n{result.stdout[-6000:]}\n{result.stderr[-6000:]}\nFake errors: {fake.errors}"
            assert not fake.errors, fake.errors
            assert fake.returned_tool and fake.saw_tool_output and fake.returned_final, f"Incomplete real tool round trip: {fake.requests}"
            assert FINAL in result.stdout, f"Final answer was not emitted:\n{result.stdout[-6000:]}"
            if harness == "claude":
                events = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
                init = next(event for event in events if event.get("type") == "system" and event.get("subtype") == "init")
                assert init["model"] == "knowwhere", f"Misleading Claude model identity: {init['model']}"

            for path, previous in before.items():
                assert digest(path) == previous, f"Saved harness settings changed: {path.name}"
            print(f"PASS {harness}: real tool execution, returned fixture output, final answer, saved settings unchanged ({len(fake.requests)} inference calls)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path(__file__).resolve().parents[1] / "target" / "debug" / "knowwhere")
    parser.add_argument("--harness", choices=("all", "claude", "codex"), default="all")
    parser.add_argument("--require-installed", action="store_true", help="Fail instead of skipping an unavailable CLI")
    parser.add_argument("--claude-normal", action="store_true", help="Linux-only: load the isolated saved Claude settings normally instead of using --bare")
    args = parser.parse_args()
    if args.claude_normal and not sys.platform.startswith("linux"):
        parser.error("--claude-normal is restricted to Linux; default --bare avoids reading a macOS keychain")
    assert args.binary.is_file(), "Build knowwhere first with cargo build"
    for harness in (("claude", "codex") if args.harness == "all" else (args.harness,)):
        executable = shutil.which(harness)
        if not executable:
            if args.require_installed:
                raise AssertionError(f"Required {harness} CLI is not installed")
            print(f"SKIP {harness}: CLI not installed (use --require-installed in harness CI)")
            continue
        exercise(args.binary.resolve(), harness, executable, claude_normal=args.claude_normal)


if __name__ == "__main__":
    main()
