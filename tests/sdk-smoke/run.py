#!/usr/bin/env python3
"""Exercise Rust Messages SSE with the official Anthropic .NET SDK and fake upstreams.

Requires .NET 10 and a built Rust executable. Every inference endpoint is loopback;
the only network dependency is restoring the pinned SDK package when uncached.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

PROJECT = Path(__file__).resolve().parent
ROOT = PROJECT.parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from differential import FakeUpstream, Gateway


class Upstream(FakeUpstream):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        failure = "ERROR" in json.dumps(body)
        if failure:
            chunks = [
                {"choices": [{"index": 0, "delta": {"content": "partial"}}]},
                {"error": {"message": "out of memory"}},
            ]
        else:
            chunks = [
                {"choices": [{"index": 0, "delta": {"reasoning_content": "Consider carefully"}}]},
                {"choices": [{"index": 0, "delta": {"content": "Hello world"}}]},
            ]
            for index, call_id, arguments in [
                (0, "call1", '{"q":'), (1, "call2", '{"q":'),
                (0, None, '"rust"}'), (1, None, '"second"}'),
            ]:
                tool = {"index": index, "function": {"arguments": arguments}}
                if call_id:
                    tool["id"] = call_id
                    tool["function"]["name"] = "lookup"
                chunks.append({"choices": [{"index": 0, "delta": {"tool_calls": [tool]}}]})
            chunks.extend([
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                {"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 5}},
            ])
        payload = b"".join(
            b"data: " + json.dumps(chunk, separators=(",", ":")).encode() + b"\n\n"
            for chunk in chunks
        )
        if not failure:
            payload += b"data: [DONE]\n\n"
        self.reply(200, payload, "text/event-stream")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dotnet", default="dotnet")
    parser.add_argument("--rust-bin", type=Path, default=ROOT / "target/debug/knowwhere")
    args = parser.parse_args()
    binary = args.rust_bin.resolve()
    if not binary.is_file():
        parser.error("Rust executable is missing; run cargo build or pass --rust-bin")
    subprocess.run([
        args.dotnet, "build", str(PROJECT / "SdkSmoke.csproj"),
        "--configuration", "Release", "--nologo",
    ], check=True, timeout=180)
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="knowwhere-sdk-") as name:
            directory = Path(name)
            url = f"http://127.0.0.1:{upstream.server_port}"
            seed = directory / "seed.json"
            seed.write_text(json.dumps({
                "defaultTier": "high", "models": [{
                    "id": "local", "provider": "local", "upstreamModelId": "up-a",
                    "enabled": True, "baseUrl": url + "/v1",
                }], "tiers": {"high": ["local"]}, "aliases": [],
            }))
            gateway = Gateway("rust", [str(binary)], directory, seed, url, os.environ.copy())
            try:
                for prompt in ["NORMAL", "ERROR"]:
                    result = subprocess.run([
                        args.dotnet, str(PROJECT / "bin/Release/net10.0/SdkSmoke.dll"),
                        gateway.base, gateway.key, prompt,
                    ], timeout=60)
                    if result.returncode:
                        raise RuntimeError(f"SDK {prompt} scenario failed")
            finally:
                gateway.close()
    finally:
        upstream.shutdown()
        upstream.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
