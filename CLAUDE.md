# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

KnowWhere is a single-user Rust model gateway (axum + SQLite) plus a session launcher for Claude Code and Codex. It is a Rust port of a .NET app ("SimpleRouter"), which is expected at `.reference/simple-router`. That checkout is gitignored and not distributed. The binary embeds its dashboard (`assets/`) and its initial catalog (`seed.json`) via `include_str!`, so edits to those files require a rebuild.

## Commands

```sh
scripts/check.sh                         # what CI runs: fmt --check, clippy -D warnings, all tests, node --check assets/app.js
cargo test --locked --all-targets        # all Rust tests
cargo test --lib routing::               # unit tests in one module
cargo test --test gateway <name>         # one integration test (tests/{gateway,launcher,responses,tls,config_cli}.rs)
cargo run -- --data-dir /tmp/kw --listen-address 127.0.0.1   # run the server against a throwaway data dir
python3 scripts/package.py --verify      # build release archive and smoke-test the extracted package
```

Optional harnesses (all upstreams are local fakes, so they use no provider credits):
- `python3 tests/sdk-smoke/run.py --rust-bin target/debug/knowwhere`: official Anthropic .NET SDK against the gateway. Needs .NET 10.
- `python3 tests/harness-smoke.py --require-installed`: real installed `claude`/`codex` CLIs against fake OpenRouter. CI pins `@anthropic-ai/claude-code@2.1.306` and `@openai/codex@0.163.0`.
- `python3 tests/differential.py --dotnet dotnet --reference-dll ... --rust-bin target/debug/knowwhere`: runs the same fixtures against the original .NET server. Needs the `.reference` checkout.

CI runs on Linux, macOS (arm and Intel) and Windows. Gate Unix-only process/terminal tests with `cfg(unix)`. Clippy warnings fail the build.

## Architecture

**Entry points.** `src/main.rs` parses the CLI (`serve` is the default; `setup`, `run`, `login`, `key`, `models refresh`, `cert create`). `Config::load` (`src/config.rs`) accepts `--flag` options, reference-style `--SimpleRouter:Section:Option=value`, and `KnowWhere__Section__Option` / `SimpleRouter__…` environment variables. `src/lib.rs` defines `AppState` (store, config, reqwest client, in-memory `Runtime`) and `app()`, which builds the router. `AppState::open` seeds an empty DB from `seed.json` and applies `seed.local.json` overrides.

**Request flow (`src/routing.rs`).** `/v1/messages` takes an Anthropic Messages request and resolves a **tier** (`max`/`high`/`mid`/`low`, `types::TIERS`). The tier comes from the `x-weave-model-class` header, or else from the catalog alias globs matched against `model`, or else `default_tier`. `select` builds a pool from that tier, keeping models that fit the context/tools requirements and have status `ok`. If the pool is empty it falls back to lower tiers only (`fallback_tiers`). Selection is sticky per session: it uses the pin for (session key, tier), then a sibling pin with the same client session, then a round-robin cursor. Local models are preferred in mid/low and limited by `max_conversations` holds. Faults (`record_fault`) set cooldowns, and `route` retries other models. If every tier is exhausted the gateway returns a 429 with `Retry-After`. If every model rejected the context length it returns a compaction-compatible 400.

**Providers (`src/providers.rs` + `src/providers/`).** There are four provider kinds: `claude` and `codex` (subscription OAuth, in `oauth.rs`), `openrouter` (API key, and every request carries a `provider.max_price` ceiling), and `local` (OpenAI-compatible `baseUrl`). Much behavior branches on `provider == "..."` strings inside `send_with_headers_and_query`. `translate.rs` converts Anthropic requests to Chat Completions/Responses. `stream.rs` converts upstream SSE back into Anthropic SSE and handles idle timeouts and keepalives. `limits.rs` parses rate-limit headers into `ModelFault`s. `discovery.rs` refreshes provider model catalogs.

**Responses bridge (`src/responses.rs`).** `/v1/responses`, used by Codex, is a stateless adapter over the same Messages router. Tools execute in the client. Stored response IDs, background jobs, hosted tools, strict schemas and grammars are rejected explicitly, and `/compact` returns 501. Reasoning replay is encrypted with a server-side key.

**Launcher (`src/launcher.rs`).** `knowwhere run claude|codex` starts a temporary loopback gateway and passes connection settings **only to the child process**. For Claude this is a private temporary settings overlay; for Codex it is `-c` overrides, and conflicting provider overrides are blocked. The launcher must never edit the harness's saved config or the shell config. It preserves the child's exit code. `setup` writes the OpenRouter route into all four tiers with $1/M prompt and completion caps.

**Persistence (`src/store.rs`).** One SQLite DB (`knowwhere.db`) holds the catalog, pins, cursors, cooldowns, usage records and secrets. Secrets are encrypted with ChaCha20-Poly1305 using the adjacent `knowwhere.key`. Request records never contain prompt/response bodies or credentials.

**Other modules.** `dashboard.rs` serves the `/ui` operator console and JSON settings endpoints. The console is reachable only over loopback, with loopback Host/Origin checks. `systemone.rs` provides decision-model endpoints and the `/v1/mcp` MCP tools; these models never join text-generation tiers. `tls.rs` generates the LAN HTTPS certificate. With a certificate configured, HTTP is restricted to loopback and only `/v1` is reachable remotely. `pricing.rs` handles cost estimation: provider-reported OpenRouter cost wins, otherwise the configured price, otherwise the cost is marked unknown.

## Conventions

- Error responses are Anthropic-shaped (`{"type":"error","error":{...}}`, via `lib::error`).
- Header names keep the reference's `x-weave-*` / `X-Weave-Router-Key` spellings for compatibility.
- `docs/porting.md` records the port's verification evidence and the intentional differences from the .NET reference. Update it when behavior deliberately diverges or when validation claims change. Test counts quoted in `README.md` and `docs/porting.md` are hand-maintained.
