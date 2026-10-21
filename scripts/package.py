#!/usr/bin/env python3
"""Build a versioned native archive and SHA-256 checksum (Python 3.12+)."""
import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent


def host_target():
    version = subprocess.check_output(["rustc", "-vV"], text=True)
    return next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))


def verify(archive, checksum, stem, executable, version):
    expected = checksum.read_text().split()[0]
    with archive.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != expected:
        raise RuntimeError("Archive SHA-256 does not match its checksum")
    with tempfile.TemporaryDirectory(prefix="knowwhere-package-") as temporary:
        directory = Path(temporary)
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as packed:
                packed.extractall(directory)
        else:
            with tarfile.open(archive) as packed:
                packed.extractall(directory, filter="data")
        binary = directory / stem / executable
        binary.chmod(0o755)
        actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
        if actual != f"knowwhere {version}":
            raise RuntimeError(f"Extracted binary has unexpected version: {actual}")
        help_text = subprocess.check_output([str(binary), "--help"], text=True)
        if "setup" not in help_text or "run" not in help_text:
            raise RuntimeError("Extracted CLI does not expose setup and run")
        for required in ["README.md", "LICENSE", "seed.json", "docs/install.md"]:
            if not (directory / stem / required).is_file():
                raise RuntimeError(f"Archive is missing {required}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target triple (defaults to rustc's host)")
    parser.add_argument("--binary", type=Path, help="Use an existing binary at this path")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target/dist")
    parser.add_argument("--release-tag", help="Require tag v<Cargo version> before packaging")
    parser.add_argument("--verify", action="store_true", help="Extract and smoke-test a native package")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if args.release_tag and args.release_tag != f"v{version}":
        parser.error(f"Release tag must match Cargo.toml: v{version}")
    target = args.target or host_target()
    if not target or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for c in target):
        parser.error("Invalid Rust target triple")
    executable = "knowwhere.exe" if "windows" in target else "knowwhere"
    if not args.skip_build and args.binary is None:
        command = ["cargo", "build", "--locked", "--release"]
        if args.target:
            command += ["--target", target]
        subprocess.run(command, cwd=ROOT, check=True)
    binary = args.binary or ROOT / "target" / (target if args.target else "") / "release" / executable
    if not binary.is_file():
        parser.error(f"Binary was not found: {binary}")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    stem = f"knowwhere-{version}-{target}"
    extension = ".zip" if "windows" in target else ".tar.gz"
    archive = args.output_dir / (stem + extension)
    with tempfile.TemporaryDirectory(prefix="knowwhere-stage-") as temporary:
        stage = Path(temporary) / stem
        stage.mkdir()
        shutil.copy2(binary, stage / executable)
        for filename in ["README.md", "LICENSE", "seed.json"]:
            shutil.copy2(ROOT / filename, stage / filename)
        shutil.copytree(ROOT / "docs", stage / "docs", ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        if extension == ".zip":
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as packed:
                for path in sorted(stage.rglob("*")):
                    if path.is_file():
                        packed.write(path, path.relative_to(stage.parent))
        else:
            with tarfile.open(archive, "w:gz") as packed:
                packed.add(stage, arcname=stem)
    checksum = archive.with_name(archive.name + ".sha256")
    with archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    checksum.write_text(f"{digest}  {archive.name}\n")
    if args.verify:
        verify(archive, checksum, stem, executable, version)
    print(archive)
    print(checksum)


if __name__ == "__main__":
    main()
