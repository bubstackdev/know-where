# Installation and release distribution

KnowWhere is one native executable with its dashboard and initial catalog embedded. End users do not need .NET, Node or Python to run it. Launching a Claude Code or Codex session requires the corresponding harness installed separately and available on PATH.

Source: [bubstackdev/know-where](https://github.com/bubstackdev/know-where). Native archives have not been published yet; source installation is available. Future archives will appear in [GitHub releases](https://github.com/bubstackdev/know-where/releases) after a maintainer publishes the release draft.

## Install from Git

With current stable Rust and a native C/C++ build toolchain:

```sh
cargo install --git https://github.com/bubstackdev/know-where.git --tag "<existing release tag>" --locked knowwhere
knowwhere --version
```

Choose a tag shown on the releases page. Until a release tag exists, omit `--tag` to install the current default branch. Cargo installs into its normal binary directory, usually `~/.cargo/bin`. Make sure that directory is on PATH. `--locked` uses the committed dependency versions. To update, choose a newer existing tag and repeat the install command with `--force`.

From an existing source checkout, use `cargo install --path . --locked`. No GitHub URL or release is needed for that path. Source builds embed the dashboard and seed; no JavaScript build step is needed.

## Install a release archive

Choose the archive matching the machine:

| Machine | Rust target in archive filename |
| --- | --- |
| macOS Apple silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux x86-64, glibc 2.35 or newer | `x86_64-unknown-linux-gnu` |
| Windows x86-64 | `x86_64-pc-windows-msvc` |

Linux archives are built on Ubuntu 22.04 and dynamically link the platform C runtime. For older distributions, other CPU architectures or musl-based systems, build from source on that platform.

Download the archive and its adjacent `.sha256` file from the same published release. In the download directory on macOS/Linux:

```sh
shasum -a 256 -c knowwhere-VERSION-TARGET.tar.gz.sha256
tar -xzf knowwhere-VERSION-TARGET.tar.gz
```

Use the actual archive filename. Windows archives are ZIP files; verify with PowerShell:

```powershell
$archive = 'knowwhere-VERSION-x86_64-pc-windows-msvc.zip'
$expected = (Get-Content "$archive.sha256").Split()[0]
$actual = (Get-FileHash $archive -Algorithm SHA256).Hash
if ($actual -ne $expected) { throw 'Checksum mismatch' }
Expand-Archive $archive
```

Place the extracted `knowwhere` or `knowwhere.exe` in a user-owned directory on PATH, then run `knowwhere --version`. Checksums detect corrupted or mismatched downloads; these archives are not code-signed or notarized.

## First session and switching back

```sh
knowwhere setup
knowwhere run claude
knowwhere run codex
```

Setup reads `OPENROUTER_API_KEY` or asks for it with a hidden terminal prompt. The key is encrypted in KnowWhere's data directory. Setup defaults to `openrouter/auto`; `--model openrouter/auto-beta` selects the beta router and `--model owner/model` selects a fixed model. Noninteractive setup needs the environment variable. All setup routes save `openRouter: {"maxPromptPrice": 1, "maxCompletionPrice": 1}` and select the model in all four tiers while preserving other catalog rows. These prices are USD per million tokens, enforced by sending `provider.max_price: {"prompt": 1, "completion": 1}` on every outbound request, including tool continuations. They cap provider token prices, not total session spend. OpenRouter filters out endpoints above either cap; KnowWhere does not retry without the caps. Adjust per-model ceilings in Settings if needed.

After upgrading an existing installation, run `knowwhere setup` again to switch its route to capped Auto. The upgrade alone leaves saved catalog choices intact. The price ceilings live in KnowWhere's per-model configuration, not an OpenRouter dashboard setting. See [OpenRouter's provider routing documentation](https://openrouter.ai/docs/guides/routing/provider-selection#max-price).

Each `run` invocation supplies temporary process settings to the harness and owns its loopback gateway. It does not write Claude/Codex or shell configuration. The harness continues to manage its ordinary session files. Run `claude` or `codex` directly for the usual subscription session afterward; no reset command is needed.

Global configuration flags go before the command, for example:

```sh
knowwhere --data-dir /path/to/separate-data setup --model owner/model
knowwhere --data-dir /path/to/separate-data run claude
```

To uninstall a Cargo installation, use `cargo uninstall knowwhere`. For an archive installation, remove the executable you placed on PATH. Uninstalling the executable keeps the data directory and saved credentials. To discard those too, delete the selected KnowWhere data directory separately after backing up anything you want to retain.

## Maintainer verification and releases

```sh
scripts/check.sh
python3 scripts/package.py --verify
```

Packaging requires Python 3.12 or newer. Archives and their `.sha256` files appear in `target/dist`. The script checks a supplied `--release-tag` against Cargo.toml, and `--verify` extracts the native archive and executes its version/help commands. Windows packaging uses the same Python entry point; `scripts/package.sh` is a POSIX convenience wrapper.

CI additionally installs the committed snapshot through Cargo's Git source (`scripts/check-install.py`) and verifies the installed executable. All GitHub Actions are pinned to full commit SHAs, and checkout does not retain repository credentials. Normal checks and RustSec scans have read-only repository permissions.

The release workflow runs when a `v*` tag is pushed. It tests and packages four native targets, verifies every archive checksum, and creates a **draft** GitHub release containing the archives, individual checksums and combined `SHA256SUMS`. Its final job alone can write repository contents. A maintainer reviews and publishes that draft on GitHub; this tooling does not push tags or publish releases from a local checkout.

Before a release, set the intended package version in Cargo.toml, commit the matching Cargo.lock, run the checks, and create a matching `vVERSION` tag. The release job rejects a tag that differs from Cargo.toml. GitHub-hosted runner labels follow the [official runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

The security workflow installs pinned `cargo-audit` 0.22.2 and checks Cargo.lock against the current RustSec advisory database on dependency changes and weekly. Informational advisories remain visible instead of being silently ignored. The development scan found no vulnerabilities and reported one unmaintained transitive dependency, `rustls-pemfile`; future advisory results may differ.
