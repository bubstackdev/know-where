#!/usr/bin/env python3
"""Offline HTTP parity checks against the C# reference. Python standard library only.

Build both programs first, then run:
  python3 tests/differential.py --dotnet /path/to/dotnet \
    --reference-dll /path/to/SimpleRouter.dll --rust-bin target/debug/knowwhere

Both gateways get fresh temporary databases and the same seed. Every upstream URL
is an in-process loopback fake; this script never uses real provider credentials.
Response IDs, timestamps and latency are deliberately excluded from comparisons.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request


def encode(value):
    return json.dumps(value, separators=(",", ":")).encode()


class FakeUpstream(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            pass  # Deliberate timeout/cancellation fixtures close keepalive sockets.

    def reply(self, status, value, content_type="application/json"):
        body = value if isinstance(value, bytes) else encode(value)
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass  # Expected when a timeout/cancellation fixture closes early.

    def do_GET(self):
        if self.path.startswith("/unhealthy/"):
            return self.reply(503, {"error": "unhealthy fixture"})
        self.reply(200, {"data": [{"id": name} for name in ["up-a", "up-b", "up-fail", "up-d3"]]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        with self.server.lock:
            self.server.requests.append((self.path, dict(self.headers), body))
        if self.path.endswith("/systemone"):
            if body.get("state") == "stall":
                time.sleep(1.5)
            if body.get("state") == "reject":
                return self.reply(400, {"error": "bad question"})
            return self.reply(200, {"model": body["model"], "answers": {"kind": {"type": "noul", "noul": 0.7}}, "usage": {"input_tokens": 120, "output_tokens": 0}})
        if not self.path.endswith("/chat/completions"):
            return self.reply(404, {"error": {"message": "Unknown fake route"}})
        if body["model"] == "up-fail":
            return self.reply(429, {"error": {"type": "rate_limit_error", "message": "rate limited"}})
        prompt = json.dumps(body.get("messages", []))
        tool = "USE_TOOL" in prompt
        reasoning = "WITH_REASONING" in prompt
        text = not tool or "WITH_TEXT" in prompt
        multiple = "MULTI_TOOL" in prompt
        message = {"role": "assistant", "content": "Hello world" if text else None}
        if tool:
            message["tool_calls"] = [{"id": "call_stable", "type": "function", "function": {"name": "lookup", "arguments": '{"q":"rust"}'}}]
            if multiple:
                message["tool_calls"].append({"id": "call_second", "type": "function", "function": {"name": "lookup", "arguments": '{"q":"second"}'}})
        if reasoning:
            message["reasoning_content"] = "Consider carefully"
        usage = {"prompt_tokens": 11, "completion_tokens": 5}
        finish = "tool_calls" if tool else "stop"
        if not body.get("stream"):
            return self.reply(200, {"id": "chat_stable", "model": body["model"], "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage})
        chunks = [{"choices": [{"index": 0, "delta": {"role": "assistant"}}]}]
        if reasoning:
            chunks.append({"choices": [{"index": 0, "delta": {"reasoning_content": "Consider carefully"}}]})
        if text:
            for fragment in ["Hello", " world"]:
                chunks.append({"choices": [{"index": 0, "delta": {"content": fragment}}]})
        if multiple:
            for index, call_id, arguments in [(0, "call_stable", '{"q":'), (1, "call_second", '{"q":'), (0, None, '"rust"}'), (1, None, '"second"}')]:
                call = {"index": index, "function": {"arguments": arguments}}
                if call_id:
                    call.update({"id": call_id, "type": "function"})
                    call["function"]["name"] = "lookup"
                chunks.append({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]})
        elif tool:
            chunks.extend([
                {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_stable", "type": "function", "function": {"name": "lookup", "arguments": '{"q":'}}]}}]},
                {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": '"rust"}'}}]}}]},
            ])
        chunks.extend([{"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}, {"choices": [], "usage": usage}])
        data = b"".join(b"data: " + encode(chunk) + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
        self.reply(200, data, "text/event-stream")


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def event_values(body):
    for event in body.decode().replace("\r\n", "\n").split("\n\n"):
        data = "\n".join(line[5:].lstrip() for line in event.splitlines() if line.startswith("data:"))
        if data and data != "[DONE]":
            yield json.loads(data)


def validate_stream(events):
    """Check Anthropic ordering separately from final message equivalence."""
    errors, opened, seen = [], {}, set()
    started, final_delta, stopped, errored = False, False, False, False
    for event in events:
        kind, index = event.get("type"), event.get("index")
        if kind == "ping":
            continue
        if stopped or errored:
            errors.append(f"{kind} appears after terminal event")
        if kind == "message_start":
            if started:
                errors.append("duplicate message_start")
            started = True
        elif kind == "error":
            errored = True
        elif not started:
            errors.append(f"{kind} appears before message_start")
        elif kind == "content_block_start":
            if opened:
                errors.append(f"block {index} starts while {sorted(opened)} remain open")
            if final_delta or index in seen or index != len(seen):
                errors.append(f"invalid block index/order: {index}")
            seen.add(index)
            opened[index] = event.get("content_block", {}).get("type")
        elif kind == "content_block_delta":
            if index not in opened:
                errors.append(f"delta for closed/unknown block {index}")
            expected = {"text_delta": "text", "thinking_delta": "thinking", "signature_delta": "thinking", "input_json_delta": "tool_use"}.get(event.get("delta", {}).get("type"))
            if expected and opened.get(index) != expected:
                errors.append(f"delta type disagrees with block {index}")
        elif kind == "content_block_stop":
            if index not in opened:
                errors.append(f"stop for closed/unknown block {index}")
            opened.pop(index, None)
        elif kind == "message_delta":
            if opened:
                errors.append("message_delta before content blocks close")
            final_delta = True
        elif kind == "message_stop":
            if opened or not final_delta:
                errors.append("message_stop before blocks close/final delta")
            stopped = True
        else:
            errors.append(f"unknown event type {kind}")
    if not stopped and not errored:
        errors.append("stream has no message_stop or error")
    return errors


def stream_message(body):
    blocks, result, usage = {}, {}, {}
    for event in event_values(body):
        kind = event.get("type")
        if kind == "message_start":
            result = {k: v for k, v in event["message"].items() if k not in {"id", "content", "usage"}}
            usage.update(event["message"].get("usage", {}))
        elif kind == "content_block_start":
            blocks[event["index"]] = dict(event["content_block"])
        elif kind == "content_block_delta":
            block, delta = blocks.setdefault(event["index"], {}), event["delta"]
            if delta["type"] == "text_delta":
                block["text"] = block.get("text", "") + delta["text"]
            elif delta["type"] == "thinking_delta":
                block["thinking"] = block.get("thinking", "") + delta["thinking"]
            elif delta["type"] == "signature_delta":
                block["signature"] = block.get("signature", "") + delta["signature"]
            elif delta["type"] == "input_json_delta":
                block["_input"] = block.get("_input", "") + delta["partial_json"]
        elif kind == "message_delta":
            result.update(event.get("delta", {}))
            usage.update(event.get("usage", {}))
        elif kind == "error":
            return {"error": event["error"].get("type")}
    for block in blocks.values():
        if "_input" in block:
            block["input"] = json.loads(block.pop("_input") or "{}")
    result["content"] = [blocks[k] for k in sorted(blocks)]
    result["usage"] = usage
    return normalize_message(result)


def normalize_message(value):
    result = {k: value.get(k) for k in ["type", "role", "model", "content", "stop_reason", "stop_sequence"]}
    usage = value.get("usage", {})
    result["usage"] = {k: usage.get(k, 0) for k in ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]}
    return result


def normalize(status, headers, body):
    if headers.get("Content-Type", "").startswith("text/event-stream"):
        # MCP can send a single JSON-RPC result through an SSE envelope.
        values = list(event_values(body))
        if values and "jsonrpc" in values[0]:
            value = values[-1]
        else:
            message = stream_message(body)
            errors = validate_stream(values)
            if errors:
                message["_stream_validation_errors"] = errors
            return status, message
    else:
        try:
            value = json.loads(body)
        except (ValueError, UnicodeDecodeError):
            return status, body.decode(errors="replace")
    if value.get("type") == "error":
        return status, {"type": "error", "error": value.get("error", {}).get("type")}
    if value.get("type") == "message":
        return status, normalize_message(value)
    if "data" in value and isinstance(value["data"], list):
        for item in value["data"]:
            if isinstance(item, dict):
                item.pop("created_at", None)
    if "jsonrpc" in value:
        result = value.get("result", {})
        if "serverInfo" in result:
            result["serverInfo"] = {"present": True}
            if "tools" in result.get("capabilities", {}):
                result["capabilities"]["tools"].setdefault("listChanged", False)
        if "error" in value:
            value["error"] = {"code": value["error"]["code"]}
        if "content" in result:
            result.setdefault("isError", False)  # Optional MCP false has identical semantics.
            for part in result["content"]:
                if part.get("type") == "text":
                    try:
                        part["text"] = json.loads(part["text"])
                    except ValueError:
                        pass
    return status, value


class Gateway:
    def __init__(self, name, command, directory, seed, upstream, environment):
        self.name, self.command, self.environment = name, command, environment
        self.port = unused_port()
        self.base = f"http://127.0.0.1:{self.port}"
        options = {
            "DataDirectory": str(directory / name), "SeedFile": str(seed),
            "ListenAddress": "127.0.0.1", "Port": str(self.port),
            "Local:HealthCheckIntervalSeconds": "0", "ModelListRefreshInterval": "00:00:00",
            "Claude:ApiBaseUrl": upstream, "Claude:AuthorizeUrl": upstream + "/authorize",
            "Claude:TokenUrl": upstream + "/token", "Codex:ApiBaseUrl": upstream + "/v1",
            "Codex:Issuer": upstream, "OpenRouter:ApiBaseUrl": upstream + "/v1",
            "Limits:KeepAliveInterval": "00:00:00", "SystemOne:MaxBodyBytes": "4096",
            "SystemOne:TimeoutSeconds": "1",
        }
        self.options = [f"--SimpleRouter:{key}={value}" for key, value in options.items()]
        key_run = subprocess.run(command + ["key", "show"] + self.options, env=environment, capture_output=True, text=True, timeout=60)
        if key_run.returncode:
            raise RuntimeError(f"{name} key initialization failed:\n{key_run.stdout}\n{key_run.stderr}")
        keys = re.findall(r"\b(?:sr|kw)_[A-Za-z0-9_-]+", key_run.stdout)
        if not keys:
            raise RuntimeError(f"{name} did not print a gateway key: {key_run.stdout}")
        self.key = keys[-1]
        self.log_path = directory / f"{name}.log"
        self.log = self.log_path.open("w")
        self.process = subprocess.Popen(command + self.options, env=environment, stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"{name} exited at startup:\n{self.log_path.read_text()[-5000:]}")
            try:
                self.request("GET", "/v1/models")
                return
            except (ConnectionError, urllib.error.URLError, TimeoutError):
                time.sleep(0.05)
        self.close()
        raise RuntimeError(f"{name} did not start:\n{self.log_path.read_text()[-5000:]}")

    def request(self, method, path, body=None, session=None, authenticated=True, extra_headers=None):
        headers = {"Content-Type": "application/json"}
        if authenticated:
            headers["x-weave-router-key"] = self.key
        if session:
            headers["X-Claude-Code-Session-Id"] = session
        headers.update(extra_headers or {})
        data = body if isinstance(body, bytes) else encode(body) if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            response = urllib.request.urlopen(request, timeout=15)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return normalize(response.status, response.headers, response.read())

    def close(self):
        if getattr(self, "process", None):
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        if getattr(self, "log", None):
            self.log.close()


def seed_catalog(upstream):
    models = [{"id": name, "provider": "local", "upstreamModelId": upstream_name, "enabled": True, "baseUrl": upstream + "/v1", "apiKey": "local-test-key"} for name, upstream_name in [("local-a", "up-a"), ("local-b", "up-b"), ("local-fail", "up-fail")]]
    models.extend([
        {"id": "claude-test", "provider": "claude", "upstreamModelId": "claude-test", "enabled": False},
        {"id": "claude-z-last", "provider": "claude", "upstreamModelId": "claude-z-last", "enabled": False},
        {"id": "claude-a-first", "provider": "claude", "upstreamModelId": "claude-a-first", "enabled": False},
        {"id": "d3", "provider": "systemone", "upstreamModelId": "up-d3", "enabled": True, "baseUrl": upstream + "/v1", "apiKey": "decision-test-key"},
        {"id": "limited-d3", "provider": "systemone", "upstreamModelId": "up-d3", "enabled": True, "baseUrl": upstream + "/v1", "decisions": "Choice, Noul, Images"},
        {"id": "unhealthy-d3", "provider": "systemone", "upstreamModelId": "up-d3", "enabled": True, "baseUrl": upstream + "/unhealthy/v1"},
        {"id": "local-small", "provider": "local", "upstreamModelId": "up-small", "enabled": True, "baseUrl": upstream + "/v1", "contextWindow": 200, "maxPromptTokens": 100, "supportsTools": False},
        {"id": "local-unpriced", "provider": "local", "upstreamModelId": "up-unpriced", "enabled": True, "baseUrl": upstream + "/v1"},
    ])
    return {"defaultTier": "high", "models": models, "tiers": {"max": ["local-small"], "high": ["local-a", "local-b"], "mid": ["local-fail", "local-b"], "low": ["local-small", "local-unpriced", "local-b"]}, "aliases": [{"pattern": "claude-*", "tier": "high"}], "prices": {name: {"input": 2, "output": 3} for name in ["local-a", "local-b", "d3", "limited-d3", "local-small"]}}


def scenarios():
    message = {"model": "high", "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]}
    decision = {"model": "d3", "state": "code", "questions": {"kind": {"type": "noul"}}}
    accept = {"Accept": "application/json, text/event-stream"}
    yield "gateway-auth", ("POST", "/v1/messages", message), {"authenticated": False}
    yield "advertised-models", ("GET", "/v1/models"), {}
    yield "advertised-models-first-page", ("GET", "/v1/models?limit=1"), {}
    for limit in ["0", "-1", "1001", "nope", "1.5", "2147483648"]:
        yield "invalid-model-page-" + limit, ("GET", "/v1/models?limit=" + limit), {}
    yield "unknown-session", ("GET", "/v1/sessions/missing/cost"), {}
    yield "duplicate-message-key", ("POST", "/v1/messages", b'{"model":"high","model":"low"}'), {}
    yield "non-object-message", ("POST", "/v1/messages", []), {}
    yield "local-token-count", ("POST", "/v1/messages/count_tokens", message), {"session": "tokens"}
    yield "local-message", ("POST", "/v1/messages", message), {"session": "pin"}
    yield "pin-reuse", ("POST", "/v1/messages", dict(message, messages=message["messages"] + [{"role": "assistant", "content": "Hi"}, {"role": "user", "content": "More"}])), {"session": "pin"}
    yield "sibling-pin", ("POST", "/v1/messages", dict(message, messages=[{"role": "user", "content": "different"}])), {"session": "pin"}
    yield "new-session-round-robin", ("POST", "/v1/messages", message), {"session": "next"}
    yield "local-stream", ("POST", "/v1/messages", dict(message, stream=True)), {"session": "stream"}
    tools = [{"name": "lookup", "description": "Lookup", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}]
    for streaming in [False, True]:
        for prompt in ["USE_TOOL", "WITH_REASONING", "WITH_REASONING_USE_TOOL", "WITH_REASONING_USE_TOOL_WITH_TEXT", "USE_TOOL_MULTI_TOOL"]:
            yield f"local-{prompt.lower()}-stream-{streaming}", ("POST", "/v1/messages", dict(message, stream=streaming, tools=tools, messages=[{"role": "user", "content": prompt}])), {"session": f"{prompt}-{streaming}"}
    yield "rate-limit-failover", ("POST", "/v1/messages", dict(message, model="mid")), {"session": "failover"}
    yield "session-cost", ("GET", "/v1/sessions/pin/cost"), {}
    yield "escaped-session-message", ("POST", "/v1/messages", message), {"session": "a/b% c"}
    yield "escaped-session-cost", ("GET", "/v1/sessions/" + urllib.parse.quote("a/b% c", safe="") + "/cost"), {}
    yield "systemone-models", ("GET", "/v1/systemone/models"), {}
    yield "systemone-decision", ("POST", "/v1/systemone", decision), {"session": "decision"}
    yield "systemone-cost", ("GET", "/v1/sessions/decision/cost"), {}
    yield "systemone-unknown", ("POST", "/v1/systemone", dict(decision, model="missing")), {}
    yield "systemone-upstream-error", ("POST", "/v1/systemone", dict(decision, state="reject")), {}
    yield "systemone-unhealthy", ("POST", "/v1/systemone", dict(decision, model="unhealthy-d3")), {}
    yield "systemone-timeout", ("POST", "/v1/systemone", dict(decision, state="stall")), {}
    yield "systemone-unsupported-question", ("POST", "/v1/systemone", dict(decision, model="limited-d3", questions={"q": {"type": "box"}})), {}
    yield "systemone-nested-video", ("POST", "/v1/systemone", dict(decision, model="limited-d3", state=[{"type": "video_url", "video_url": {"url": "https://fake.invalid/video"}}])), {}
    yield "systemone-duplicate-key", ("POST", "/v1/systemone", b'{"model":"d3","model":"missing"}'), {}
    yield "systemone-body-limit", ("POST", "/v1/systemone", dict(decision, state="x" * 5000)), {}
    yield "tool-capability-filter", ("POST", "/v1/messages", dict(message, model="low", tools=tools)), {"session": "unpriced"}
    yield "unpriced-local-cost", ("GET", "/v1/sessions/unpriced/cost"), {}
    yield "prompt-cap-fallback", ("POST", "/v1/messages", dict(message, model="low", messages=[{"role": "user", "content": "x" * 600}])), {"session": "prompt-cap"}
    yield "context-exhaustion", ("POST", "/v1/messages", dict(message, model="max", messages=[{"role": "user", "content": "x" * 1000}])), {"session": "context"}
    yield "valid-tier-header", ("POST", "/v1/messages", message), {"session": "header", "extra_headers": {"x-weave-model-class": "max"}}
    yield "invalid-tier-header", ("POST", "/v1/messages", message), {"extra_headers": {"x-weave-model-class": "nope"}}
    yield "mcp-initialize", ("POST", "/v1/mcp", {"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "clientInfo": {"name": "differential", "version": "1"}, "capabilities": {}}}), {"extra_headers": accept}
    yield "mcp-notification", ("POST", "/v1/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"}), {"extra_headers": accept}
    yield "mcp-logging-level", ("POST", "/v1/mcp", {"jsonrpc": "2.0", "id": 9, "method": "logging/setLevel", "params": {"level": "warning"}}), {"extra_headers": accept}
    yield "mcp-unknown-method", ("POST", "/v1/mcp", {"jsonrpc": "2.0", "id": 8, "method": "missing"}), {"extra_headers": accept}
    yield "mcp-parse-error", ("POST", "/v1/mcp", b'{"jsonrpc":"2.0",bad'), {"extra_headers": accept}
    for name, arguments in [("systemone_models", {}), ("systemone_decide", decision)]:
        yield "mcp-" + name, ("POST", "/v1/mcp", {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}), {"session": "mcp", "extra_headers": accept}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dotnet", default=shutil.which("dotnet"))
    parser.add_argument("--reference-dll", type=Path, required=True)
    parser.add_argument("--rust-bin", type=Path, default=Path("target/debug/knowwhere"))
    parser.add_argument("--keep", action="store_true", help="Keep temporary databases and logs")
    args = parser.parse_args()
    if not args.dotnet:
        parser.error("dotnet was not found; pass --dotnet")
    for path in [args.reference_dll, args.rust_bin]:
        if not path.is_file():
            parser.error(f"Executable/assembly does not exist: {path}")
    directory = Path(tempfile.mkdtemp(prefix="knowwhere-differential-"))
    environment = {k: v for k, v in os.environ.items() if not k.startswith(("SimpleRouter__", "KnowWhere__", "KNOW_WHERE__", "ASPNETCORE_"))}
    environment.update({"DOTNET_NOLOGO": "1", "DOTNET_CLI_HOME": str(directory / "dotnet-home"), "DOTNET_SKIP_FIRST_TIME_EXPERIENCE": "1", "RUST_LOG": "warn"})
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeUpstream)
    upstream.requests, upstream.lock = [], threading.Lock()
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    gateways, failures, count = [], [], 0
    try:
        base = f"http://127.0.0.1:{upstream.server_port}"
        seed = directory / "seed.json"
        seed.write_text(json.dumps(seed_catalog(base)))
        gateways.append(Gateway("reference", [args.dotnet, str(args.reference_dll.resolve())], directory, seed, base, environment))
        gateways.append(Gateway("rust", [str(args.rust_bin.resolve())], directory, seed, base, environment))
        for name, positional, keywords in scenarios():
            results = [gateway.request(*positional, **keywords) for gateway in gateways]
            # The reference commits Messages usage in OnCompleted, after the
            # response reaches the client. Compare eventual accounting, bounded
            # tightly enough to expose dropped or duplicated usage records.
            if positional[0] == "GET" and positional[1].endswith("/cost"):
                deadline = time.monotonic() + 5
                while results[0] != results[1] and time.monotonic() < deadline:
                    time.sleep(0.05)
                    results = [gateway.request(*positional, **keywords) for gateway in gateways]
            count += 1
            invalid_stream = any(isinstance(result[1], dict) and result[1].get("_stream_validation_errors") for result in results)
            if results[0] != results[1] or invalid_stream:
                failures.append(name)
                print(f"FAIL {name}\n  reference: {json.dumps(results[0], sort_keys=True)}\n  rust:      {json.dumps(results[1], sort_keys=True)}", flush=True)
            else:
                print(f"PASS {name}", flush=True)
        # Identical pin assignment and concurrent sibling assignment must each
        # consume exactly one round-robin turn. Arrival order may vary, so sort
        # equivalent replies before comparing and then check the following turn.
        for sibling in [False, True]:
            name = "concurrent-sibling-pins" if sibling else "concurrent-same-pin"
            results = []
            for gateway in gateways:
                def send(index):
                    prompt = f"side-{index}" if sibling else "same"
                    body = {"model": "high", "max_tokens": 32, "messages": [{"role": "user", "content": prompt}]}
                    return gateway.request("POST", "/v1/messages", body, session=name)
                with ThreadPoolExecutor(max_workers=8) as pool:
                    replies = list(pool.map(send, range(8)))
                results.append(sorted(replies, key=lambda r: json.dumps(r, sort_keys=True)))
            count += 1
            if results[0] != results[1] or len({reply[1].get("model") for reply in results[1]}) != 1:
                failures.append(name)
                print(f"FAIL {name}\n  reference: {json.dumps(results[0], sort_keys=True)}\n  rust: {json.dumps(results[1], sort_keys=True)}", flush=True)
            else:
                print(f"PASS {name}", flush=True)
            follow = {"model": "high", "max_tokens": 32, "messages": [{"role": "user", "content": "following"}]}
            results = [gateway.request("POST", "/v1/messages", follow, session=name + "-next") for gateway in gateways]
            count += 1
            if results[0] != results[1]:
                failures.append(name + "-next-turn")
                print(f"FAIL {name}-next-turn: {results}", flush=True)
            else:
                print(f"PASS {name}-next-turn", flush=True)
        print(f"\n{count - len(failures)}/{count} differential scenarios passed.")
        return 1 if failures else 0
    finally:
        for gateway in reversed(gateways):
            gateway.close()
        upstream.shutdown()
        upstream.server_close()
        if args.keep:
            print(f"Logs and temporary databases: {directory}")
        else:
            shutil.rmtree(directory)


if __name__ == "__main__":
    sys.exit(main())
