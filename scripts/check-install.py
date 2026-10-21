#!/usr/bin/env python3
"""Exercise cargo's Git installation path against the committed checkout."""
from pathlib import Path
import os
import subprocess
import tempfile
import tomllib

root = Path(__file__).resolve().parent.parent
revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
with tempfile.TemporaryDirectory(prefix="know-where-install-") as temporary:
    subprocess.run([
        "cargo", "install", "--git", root.as_uri(), "--rev", revision,
        "--locked", "--root", temporary, "know-where",
    ], check=True, cwd=root)
    binary = Path(temporary) / "bin" / ("know-where.exe" if os.name == "nt" else "know-where")
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if actual != f"know-where {version}":
        raise RuntimeError(f"Installed version is incorrect: {actual}")
    help_text = subprocess.check_output([str(binary), "--help"], text=True)
    if "setup" not in help_text or "run" not in help_text:
        raise RuntimeError("Installed CLI does not expose setup and run")
    print(f"Verified Git installation at {binary}")
