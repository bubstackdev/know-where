# Repository Guidelines

## Project Structure & Module Organization

KnowWhere is a single-user Rust model gateway and reversible launcher for Claude Code and Codex. `src/main.rs` handles the CLI; `src/lib.rs` assembles the Axum application. Routing, persistence, configuration, TLS, and Responses translation have dedicated modules in `src/`; provider integrations live in `src/providers/`.

`assets/` contains the embedded HTML, CSS, and JavaScript dashboard. `seed.json` supplies the initial catalog. Rust integration tests and optional Python/.NET harnesses live in `tests/`. `scripts/` contains verification and packaging tools; `docs/` covers installation and port validation.

## Build, Test, and Development Commands

- `cargo build --locked`: build the debug binary.
- `cargo run --locked -- --data-dir /tmp/knowwhere-dev --listen-address 127.0.0.1`: run locally with isolated development data.
- `scripts/check.sh`: check Rust formatting, deny Clippy warnings, run all Rust test targets, and check dashboard JavaScript syntax. Requires Node for the syntax check.
- `cargo test --locked --test gateway`: run gateway integration tests only.
- `python3 scripts/package.py --verify`: build and verify a release package.

The application itself requires neither Node nor .NET. Optional differential and SDK harness prerequisites and commands are documented in `README.md`.

## Coding Style & Naming Conventions

Use Rust 2024 conventions, four-space indentation, and `cargo fmt --all`. Name modules and functions in `snake_case`, types in `PascalCase`, and constants in `SCREAMING_SNAKE_CASE`. Follow neighboring code for dashboard assets and Python scripts. Keep provider-specific translation in `src/providers/` and ensure changes pass Clippy with warnings denied.

## Testing Guidelines

Use Rust's test harness and `#[tokio::test]` for asynchronous cases. Add descriptive behavior-based tests to the relevant integration file, such as `tests/gateway.rs`, `tests/responses.rs`, or `tests/launcher.rs`. Use local fake upstreams and temporary data directories. Cover affected routing, streaming, error, and persistence behavior. No numeric coverage threshold is configured.

## Commit & Pull Request Guidelines

Recent commits use imperative, sentence-case subjects, such as “Fall back to lower model tiers when the requested tier is exhausted.” Keep commits focused. In pull requests, describe the behavior change, relevant issues, and verification performed; include screenshots for dashboard changes.

## Security & Agent Instructions

Keep credentials and runtime databases out of commits. Use isolated development data instead of the personal `~/.knowwhere` directory. Never suspend, terminate, or alter other terminal sessions or their processes; control only jobs started by the current session.
