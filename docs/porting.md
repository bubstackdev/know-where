# Rust port verification

`know-where` rebuilds the behavior of a local `.reference/simple-router` checkout as a Rust application. That reference is not distributed in this repository. Its source and test cases informed Rust regression tests and an offline comparison harness.

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
- Installed Claude Code 2.1.306 and Codex 0.163.0: real tool-read / tool-output / final-answer sessions through fake OpenRouter, with saved harness config and synthetic auth files unchanged.

Run `scripts/check.sh` for the Rust checks. `tests/differential.py` and `tests/sdk-smoke/run.py` expose command-line arguments for running the optional .NET comparisons. Their upstream endpoints are local fakes; they consume no provider inference credits.

The 162-test total is for macOS; Unix-only process and terminal tests are not counted on Windows.

The original 719 tests have not all been mechanically translated or run directly against the Rust server. Differential and SDK tests provide independent coverage, rather than a claim that every internal .NET test has a corresponding Rust test.

## Intentional implementation differences

- The executable and default data directory are renamed to `know-where` and `~/.know-where`.
- SQLite uses a new schema. Credentials use authenticated ChaCha20-Poly1305 encryption with an owner-only key file. Existing EF Core databases and ASP.NET Data Protection credentials are not imported; fresh logins are required.
- The dashboard uses bundled HTML/CSS/JavaScript and local JSON settings endpoints instead of Blazor Server. Monitoring refreshes periodically. Settings mutations read current state and change the selected section.
- Generated certificates use ECDSA P-256 instead of RSA-2048. They retain the single-use constrained CA, local identities, 397-day validity and encrypted server key. Supplied PKCS#12 import requires a system `openssl` executable; generated certificates and normal HTTPS serving do not.
- Prices retain the current floating-point JSON storage shape, then use decimal arithmetic over the stored decimal representation for micro-USD rounding and saturation.

## Validation limits

Live Claude/Codex authentication and paid OpenRouter calls have not been exercised with user accounts. Real installed Claude Code and Codex are tested with local fake inference endpoints and fixture credentials. OAuth and protocol behavior are covered with local fixtures, including refresh cancellation and concurrent credential rejection.

Cross-platform CI is configured for Linux, macOS and Windows, but only macOS was executed locally. The application remains a single-user gateway; proxying a remote dashboard connection through loopback would defeat its direct-connection locality boundary.
