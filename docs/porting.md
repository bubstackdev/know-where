# Rust port verification

`knowwhere` rebuilds the behavior of a local `.reference/simple-router` checkout as a Rust application. That reference is not distributed in this repository. Its source and test cases informed Rust regression tests and an offline comparison harness.

## Implemented scope

| Area | Implementation |
| --- | --- |
| Gateway | Anthropic Messages and token-count endpoints, stateless Responses bridge, model listing, gateway key creation/rotation, client header compatibility |
| Harness sessions | OpenRouter setup, temporary Claude settings overlay, Codex provider overrides, private loopback gateway, exit-code and signal handling, cleanup without saved harness configuration edits |
| Routing | Four tiers, ordered aliases, persistent rotation and pins, sibling-session affinity, tool/context eligibility, local conversation capacity, same-tier failover and persisted cooldowns |
| Providers | Claude subscription, Codex subscription/Responses, local Chat Completions, OpenRouter fixed and automatic routing |
| Credentials and discovery | PKCE and device login, refresh serialization and rotation persistence across cancellation, model discovery and last-successful caches, OpenRouter capability/pricing updates |
| Streaming | Sequential Anthropic content blocks, fragmented/interleaved tools, signed reasoning round trips, initial-output grace, configurable keepalives, byte-activity idle timeouts and terminal-event validation |
| Accounting | Usage and cache tokens, exact decimal micro-USD rounding, reported/estimated/unknown provenance, saturated totals, cancellation and completion diagnostics |
| Dashboard | Monitoring, charts, accounts, model CRUD, ordered tiers, aliases, prices, gateway keys, request details and cursor-paged pin history |
| Decision models | Explicit SystemOne HTTP proxy and stateless Streamable HTTP MCP tools, model/media capabilities, health and usage |
| Networking | HTTP/HTTPS listeners, constrained local CA/server certificate generation, supplied certificates, direct TCP loopback dashboard boundary and Host/Origin validation |
| Operations | OpenRouter setup, reversible Claude/Codex session launchers, CLI/environment overrides, changed-content local seeds, bundled assets/catalog, release archives and CI |

## Validation performed

On macOS ARM64:

- Original .NET suite: **717 unit tests and 2 acceptance tests passed**.
- Rust release checks: formatting, Clippy with warnings denied, JavaScript syntax, and **162 Rust tests passed** (97 library, 6 launcher unit, 15 CLI/startup, 18 gateway, 7 launcher process/terminal, 9 Responses, 10 TLS/security).
- Offline differential harness: **60 scenarios passed** against the original server and the optimized Rust release executable. The harness checks HTTP status, semantic response shapes, accounting, concurrency and independent SSE lifecycle validation.
- Official Anthropic .NET SDK: successful aggregation of thinking, text, parallel fragmented tool calls and usage; committed stream errors produce the SDK's expected exception.
- Real HTTPS handshakes: generated trust chain, DNS/IP identities, rejection of incorrect identity/trust, supplied PEM and password-protected PKCS#12.
- Safari: model creation, tier saves, charts, failed-stream request details and pin-history navigation.
- Installed Claude Code 2.1.306 and Codex 0.163.0: actual local tool execution, tool output returned to the fake OpenRouter, final answer, and unchanged fixture settings/credentials. Conflicting Claude session settings are overridden by a private temporary overlay; no provider secrets appear in child arguments.
- Focused review regressions: interactive Ctrl-C preserves the session, noninteractive termination closes tools/listener, reserved setup IDs are preserved, Codex provider override variants are blocked, reasoning replay uses a server-only persistent key, and truncated tools report incomplete output.
- RustSec audit: no known vulnerabilities; one informational unmaintained notice for transitive `rustls-pemfile`. GitHub workflows also pass actionlint.

Run `scripts/check.sh` for the Rust checks. `tests/differential.py` and `tests/sdk-smoke/run.py` expose command-line arguments for running the optional .NET comparisons. Their upstream endpoints are local fakes; they consume no provider inference credits.

The 162-test total is for macOS; Unix-only process and terminal tests are not counted on Windows.

[GitHub CI on commit `aa75330`](https://github.com/bubstackdev/knowwhere/actions/runs/39673115918) passed Linux, Apple silicon and Intel macOS, and Windows, including installation through Cargo's Git source and archive extraction checks. Those runs used the earlier executable spelling. The KnowWhere rename also passed the local Rust, SDK, real-harness, differential and archive checks; CI checks each pushed revision. Native archives have not been published.

## Live OpenRouter validation

On 2026-10-21, installed Claude Code and Codex each completed a live local-tool round trip through OpenRouter using `xiaomi/mimo-v2.6-pro-ultraspeed`: tool request, actual local execution, returned tool output and final answer. Each harness made two inference API calls.

| Validation | Reported cost (USD) |
| --- | ---: |
| Claude Code, two inference calls | 0.00419878 |
| Codex, two inference calls | 0.03568091 |
| Discovery | 0.00018270 |
| **Total** | **0.04006239** |

The capped Auto setup and Claude model identity fix were separately validated on the same date with Claude Code 2.1.307. A live Read-tool round trip through `openrouter/auto` resolved to `deepseek/deepseek-v4-flash-0731`, made two inference calls, and reported $0.00035406 for 1,793 prompt and 448 completion tokens. Both calls carried `provider.max_price: {"prompt": 1, "completion": 1}`. An isolated interactive terminal check also confirmed that `/model` selects **KnowWhere** and labels family choices as gateway routes. The current local Rust suite has 163 passing tests; the harness smoke test checks Claude's startup model identity and request caps as well as tool execution.

These are the recorded validation charges, not a quote for future sessions. No API keys or credential data are included in the repository or this record.

The original 719 tests have not all been mechanically translated or run directly against the Rust server. Differential and SDK tests provide independent coverage, rather than a claim that every internal .NET test has a corresponding Rust test.

## Intentional implementation differences

- The executable and default data directory are renamed to `knowwhere` and `~/.knowwhere`.
- SQLite uses a new schema. Credentials use authenticated ChaCha20-Poly1305 encryption with an owner-only key file. Existing EF Core databases and ASP.NET Data Protection credentials are not imported; fresh logins are required.
- The dashboard uses bundled HTML/CSS/JavaScript and local JSON settings endpoints instead of Blazor Server. Monitoring refreshes periodically. Settings mutations read current state and change the selected section.
- Generated certificates use ECDSA P-256 instead of RSA-2048. They retain the single-use constrained CA, local identities, 397-day validity and encrypted server key. Supplied PKCS#12 import requires a system `openssl` executable; generated certificates and normal HTTPS serving do not.
- Prices retain the current floating-point JSON storage shape, then use decimal arithmetic over the stored decimal representation for micro-USD rounding and saturation.

## Validation limits

Claude/Codex subscription OAuth login and refresh still require account-specific live testing. Their protocol behavior is covered with local fixtures, including refresh cancellation and concurrent credential rejection. Real installed harnesses have passed both local fake-provider tests and the live OpenRouter validation recorded above.

Local validation ran on macOS, with additional platforms exercised by GitHub CI as recorded above. The application remains a single-user gateway; proxying a remote dashboard connection through loopback would defeat its direct-connection locality boundary.
