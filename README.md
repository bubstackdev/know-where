# know-where

A reversible session launcher for Claude Code and Codex, backed by a single-user Rust model gateway. Run a session through OpenRouter, then use your usual `claude` or `codex` command to return to your subscription.

Developed against the application in the local `.reference/simple-router` checkout, which is not distributed with this repository. The Rust binary embeds its dashboard assets and initial catalog; no .NET or Node runtime is required to run it.

## Install

With a current stable Rust toolchain:

```sh
cargo install --git https://github.com/bubstackdev/know-where.git --locked know-where
```

For a published release, pin the existing version tag:

```sh
cargo install --git https://github.com/bubstackdev/know-where.git --tag "<existing release tag>" --locked know-where
```

From a checkout, use `cargo install --path . --locked`. Versioned macOS, Linux and Windows archives and SHA-256 checksums are produced by the release workflow and appear in [published GitHub releases](https://github.com/bubstackdev/know-where/releases). See [installation and releases](docs/install.md) for archive selection, checksum verification, updates and maintainer commands.

## Start an OpenRouter session

Have the `claude` and/or `codex` CLI installed and on your PATH. Choose an OpenRouter model ID and run:

```sh
know-where setup --model owner/model
know-where run claude
# or:
know-where run codex
```

Replace `owner/model` with a model that supports tool calls for coding sessions. Setup reads `OPENROUTER_API_KEY` when present; otherwise it asks for the key without echoing it. Without `--model`, interactive setup also asks for the model. Setup saves the key encrypted and selects that OpenRouter model in all four routing tiers, keeping other model entries available in Settings.

`run` starts a temporary loopback gateway and supplies connection settings only to the child process. Know-Where does not edit Claude/Codex configuration files or your shell configuration. The harness can still write its normal session state and history. The gateway stops when the child exits, and the child's exit code is preserved.

Pass harness arguments after `--`:

```sh
know-where run claude -- -p "Explain this repository"
know-where run codex -- exec "Explain this repository"
```

To use your usual account again, launch `claude` or `codex` directly. To select another OpenRouter model, run `know-where setup --model another-owner/model` before the next session. Setup modifies Know-Where's routing configuration; it does not modify the harness's saved configuration.

## Run the persistent gateway

For subscription routing, local models, the dashboard or LAN clients:

```sh
know-where login claude
know-where login codex
know-where key show
know-where
```

Open `http://localhost:8790/ui` for monitoring or `/ui/settings` to edit settings. The dashboard accepts only connections from a loopback address with a loopback Host and a same-port loopback Origin, when present. Gateway API routes require the generated gateway key.

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8790
export ANTHROPIC_AUTH_TOKEN="$(know-where key show)"
```

Clients may supply the key through `x-api-key`, `Authorization: Bearer`, or `X-Weave-Router-Key`.

## Routing

The `x-weave-model-class` header selects `max`, `high`, `mid`, or `low`. Without it, the requested model is matched against ordered aliases, then the default tier is used. Responses identify their tier through `X-Weave-Model-Class`.

New sessions rotate through eligible models. Session pins and rotation cursors survive restarts. Separate first user messages create separate pins; pins belonging to the same client session start with its eligible model in that tier. Mid and low tiers prefer local models. Local models may limit concurrent conversations and reject prompts above their configured size. Token-count requests do not create pins or consume rotation turns.

Before output begins, rate limits, provider failures, and context overflows may move a request to another model in the same tier. A committed response never fails over. Exhausted tiers return an Anthropic-shaped 429 with `Retry-After`; if every attempted model rejects context length, the gateway returns a compaction-compatible 400.

## Configuration and storage

```sh
know-where --data-dir /path/to/data --listen-address 127.0.0.1 --port 8790
know-where --SimpleRouter:Local:StreamIdleTimeout=00:02:00
```

Reference-style `--SimpleRouter:Section:Option=value` options remain accepted. Environment equivalents use `KnowWhere__Section__Option`, for example `KnowWhere__Port=8790` and `KnowWhere__Local__StreamIdleTimeout=00:02:00`. `SimpleRouter__` is also accepted. Run `know-where --help` for CLI commands.

The default data directory is `~/.know-where`. `know-where.db` contains the SQLite catalog, routing state, encrypted secrets, and request accounting. The adjacent `know-where.key` encrypts secrets; back up both together. Request records include model, token counts, cost provenance, timing and completion status. Prompt/response bodies and credentials are not retained in request history.

`seed.json` initializes an empty catalog. A `seed.local.json` in the data directory can override configured model fields:

```json
{
  "models": [
    {
      "id": "mimo-2.6",
      "enabled": true,
      "baseUrl": "http://127.0.0.1:8001/v1",
      "upstreamModelId": "your-served-model"
    }
  ]
}
```

Overrides apply when the file content changes, so subsequent dashboard edits survive restarts. Unknown model IDs are skipped. Keep real endpoint keys in this local file or enter them in Settings; the shipped catalog contains disabled local placeholders. OpenRouter uses its separately managed provider key.

Prices are estimates in USD per million tokens. Provider-reported OpenRouter costs take precedence; otherwise the configured resolved model's price is used. Unpriced responses are marked unknown, and session totals count unknown-cost requests separately.

Existing `.simple-router` databases and ASP.NET-encrypted credentials are not imported. Start with fresh logins and configure the Rust catalog.

## LAN HTTPS

```sh
know-where cert create
know-where
```

Certificate creation prints a LAN URL, the CA file to trust on other machines, and its SHA-256 fingerprint. It generates a CA restricted to local DNS names, signs one server certificate, and discards the CA's private key. Certificates last 397 days. The server private key is encrypted in the database. After renewal, restart the server and trust the new CA on clients.

Without a certificate, the default `0.0.0.0:8790` listener serves plain HTTP on the LAN. Set `--listen-address 127.0.0.1` for local-only use. With a certificate, HTTP is restricted to loopback and HTTPS listens on the configured address at port 8791. Only `/v1` routes are accessible remotely. Keep the dashboard directly connected: a loopback reverse proxy would make its remote clients look local.

A supplied certificate is accepted through `--SimpleRouter:Https:CertificatePath=/path/certificate.p12` and optional `--SimpleRouter:Https:Password=...`. PEM certificate/key bundles are also accepted; PKCS#12 imports use the system `openssl` executable.

## API

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/messages` | Streaming or non-streaming Anthropic Messages |
| `POST /v1/responses` | Responses bridge used by Codex sessions |
| `POST /v1/responses/compact` | Explicit 501; the launcher uses client-managed compaction |
| `POST /v1/messages/count_tokens` | Provider token count or local estimate |
| `GET /v1/models` | Advertised Claude aliases representing enabled tiers |
| `GET /v1/subscriptions/usage` | Subscription exhaustion and credential states |
| `GET /v1/sessions/{client_session_id}/cost` | Session request count, known cost and cost provenance counts |
| `POST /v1/systemone` | Explicit decision-model calls |
| `GET /v1/systemone/models` | Decision-model health and capabilities |
| `POST /v1/mcp` | Stateless Streamable HTTP MCP decision tools |

```sh
claude mcp add --transport http know-where http://127.0.0.1:8790/v1/mcp \
  --header "x-api-key: YOUR_GATEWAY_KEY"
```

SystemOne models never join text-generation tiers. Their configured capabilities gate choice, yes/no, score, box, image and video inputs.

The Responses bridge serves client-managed conversation history and client-executed tools. Platform-stored response IDs, background jobs, hosted tools/search, strict schema enforcement and custom tool grammars return explicit errors. The Codex launcher disables hosted search and remote compaction for the session; normal local file and command tools remain in the harness.

## Verification and packaging

```sh
scripts/check.sh
python3 scripts/package.py --verify
```

Tests use local fake upstreams and temporary data directories. If you have the original .NET checkout at `.reference/simple-router`, the optional differential harness runs both servers against the same fixtures:

```sh
dotnet build .reference/simple-router/SimpleRouter.slnx -c Release
cargo build
python3 tests/differential.py \
  --dotnet dotnet \
  --reference-dll .reference/simple-router/src/SimpleRouter/bin/Release/net10.0/SimpleRouter.dll \
  --rust-bin target/debug/know-where
```

The official Anthropic .NET SDK is also exercised against the Rust gateway with local fake upstreams:

```sh
python3 tests/sdk-smoke/run.py --dotnet dotnet --rust-bin target/debug/know-where
```

That test verifies SDK aggregation of signed thinking, text, fragmented parallel tool calls, usage and stream errors. It requires .NET 10 only for the test harness. Installed harness acceptance tests use fake OpenRouter endpoints:

```sh
python3 tests/harness-smoke.py --require-installed
```

CI checks formatting, Clippy, Rust tests, dashboard JavaScript, installation through Cargo's Git source and extracted release packages on Linux, macOS and Windows. Dependency scanning runs on lockfile changes and weekly. Live provider authentication and paid OpenRouter calls still require user-account testing.

See [Rust port verification](docs/porting.md) for the scope, test evidence and intentional implementation differences.

Licensed under the [MIT license](LICENSE).
