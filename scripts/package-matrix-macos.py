#!/usr/bin/env python3
"""Assemble a portable Matrix package from an existing native Codex package."""

import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = "aarch64-apple-darwin"
LAUNCHER = """#!/bin/sh
set -eu
matrix_path="$0"
while [ -L "$matrix_path" ]; do
    matrix_dir="$(CDPATH= cd -P -- "$(dirname -- "$matrix_path")" && pwd)"
    matrix_path="$(readlink "$matrix_path")"
    case "$matrix_path" in
        /*) ;;
        *) matrix_path="$matrix_dir/$matrix_path" ;;
    esac
done
matrix_dir="$(CDPATH= cd -P -- "$(dirname -- "$matrix_path")" && pwd)"
export CODEX_MATRIX_RAIN=1
exec "$matrix_dir/codex" -c 'model="gpt-6.1-sol"' -c 'model_reasoning_effort="ultra"' "$@"
"""


def assemble(source, output):
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        raise ValueError("Native Apple Silicon Mac required")
    metadata = json.loads((source / "codex-package.json").read_text())
    version = metadata["version"]
    if not isinstance(version, str) or not re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version
    ):
        raise ValueError("Invalid stable package version")
    expected = {
        "layoutVersion": 1,
        "target": TARGET,
        "variant": "codex",
        "entrypoint": "bin/codex",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    if any(metadata.get(key) != value for key, value in expected.items()):
        raise ValueError("Package metadata mismatch")
    for relative in ("bin/codex", "bin/codex-code-mode-host", "codex-path/rg"):
        binary = source / relative
        if not binary.is_file() or not binary.stat().st_mode & 0o111:
            raise ValueError(f"Missing package executable: {relative}")
    if not (source / "codex-resources").is_dir():
        raise ValueError("Package resources missing")
    package_name = f"codex-matrix-{version}-{TARGET}"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / (package_name + ".tar.gz")
    if archive.exists() or archive.is_symlink():
        raise ValueError("Refusing to overwrite an existing package archive")
    with tempfile.TemporaryDirectory(prefix="matrix-package-") as temporary:
        package = Path(temporary) / package_name
        shutil.copytree(source, package, symlinks=True)
        for name in ("LICENSE", "NOTICE"):
            shutil.copy2(ROOT / name, package / name)
        shutil.copy2(
            ROOT / "scripts/install-matrix-macos.sh", package / "install.command"
        )
        (package / "install.command").chmod(0o755)
        wrapper = package / "bin/codex-matrix"
        wrapper.write_text(LAUNCHER)
        wrapper.chmod(0o755)
        (package / "INSTALL.md").write_text(
            "# 다른 맥에 설치\n\n"
            "- macOS Apple Silicon 전용 패키지다.\n"
            "- Rust·Node.js·Python 설치 없이 실행한다.\n"
            "- 압축을 풀고 해당 폴더에서 `sh ./install.command`를 실행한다.\n"
            f"- 설치 뒤 `~/.codex/bin/codex-matrix-{version}`를 실행한다.\n"
            "- 설치 위치를 바꾸려면 `sh ./install.command /원하는/경로`를 실행한다.\n"
            "- 설치하지 않고 `./bin/codex-matrix`를 실행해도 된다.\n"
            "- 기존 Codex 설정·인증·실행 명령은 보존한다.\n"
            "- 처음 사용하는 맥에서는 Codex 로그인을 진행한다.\n"
            "- 버전별로 설치한다. 새 패키지는 별도로 설치한다.\n"
        )
        subprocess.run(
            ["codesign", "--verify", "--strict", str(package / "bin/codex")],
            check=True,
        )
        actual = subprocess.check_output([str(wrapper), "--version"], text=True).strip()
        if actual != f"codex-cli {version}":
            raise ValueError("CLI version mismatch")
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(package, arcname=package_name)
    hasher = hashlib.sha256()
    with archive.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(block)
    digest = hasher.hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n"
    )
    print(archive)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assemble(args.package.resolve(), args.output.resolve())
