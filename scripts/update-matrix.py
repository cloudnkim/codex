#!/usr/bin/env python3
"""Install verified public Matrix releases without blocking the existing CLI."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request

TARGET = "aarch64-apple-darwin"
API = "https://api.github.com/repos/cloudnkim/codex/releases/latest"
HOSTS = {
    "api.github.com",
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
    "github-releases.githubusercontent.com",
}


def trusted_url(url):
    parsed = urllib.parse.urlsplit(url)
    if (
        parsed.scheme != "https"
        or parsed.hostname not in HOSTS
        or parsed.port not in (None, 443)
        or parsed.username
        or parsed.password
    ):
        raise ValueError("Untrusted download URL")
    return url


class TrustedRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        return super().redirect_request(
            request, fp, code, message, headers, trusted_url(newurl)
        )


def open_url(url):
    request = urllib.request.Request(
        trusted_url(url),
        headers={
            "User-Agent": "codex-matrix-updater",
            "Accept": "application/vnd.github+json",
        },
    )
    return urllib.request.build_opener(TrustedRedirect()).open(request, timeout=20)


def version_tuple(value):
    if not isinstance(value, str) or not re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value
    ):
        raise ValueError("Invalid stable version")
    return tuple(map(int, value.split(".")))


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def download(asset, destination):
    expected = asset.get("digest")
    size = asset.get("size")
    if (
        not isinstance(expected, str)
        or not re.fullmatch(r"sha256:[0-9a-fA-F]{64}", expected)
        or not isinstance(size, int)
        or size <= 0
    ):
        raise ValueError("Missing asset integrity metadata")
    count = 0
    with (
        open_url(asset["browser_download_url"]) as response,
        destination.open("xb") as output,
    ):
        while chunk := response.read(1024 * 1024):
            count += len(chunk)
            if count > size:
                raise ValueError("Asset exceeds advertised size")
            output.write(chunk)
    if count != size or "sha256:" + digest(destination) != expected.lower():
        raise ValueError("Asset integrity mismatch")


def extract(archive, destination, folder):
    root = destination.resolve()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            name = PurePosixPath(member.name)
            if (
                name.is_absolute()
                or ".." in name.parts
                or not name.parts
                or name.parts[0] != folder
            ):
                raise ValueError("Unsafe package path")
            target = root / member.name
            if member.issym() or member.islnk():
                link = PurePosixPath(member.linkname)
                parent = target.parent if member.issym() else root
                if link.is_absolute() or not (
                    parent / member.linkname
                ).resolve().is_relative_to(root / folder):
                    raise ValueError("Unsafe package link")
            elif not member.isfile() and not member.isdir():
                raise ValueError("Unsupported package member")
        # The data filter also checks links against members extracted earlier.
        tar.extractall(destination, filter="data")


def validate_package(package, version):
    if package.is_symlink() or not package.is_dir():
        raise ValueError("Invalid package root")
    metadata = json.loads((package / "codex-package.json").read_text())
    expected = {
        "layoutVersion": 1,
        "version": version,
        "target": TARGET,
        "variant": "codex",
        "entrypoint": "bin/codex",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    if any(metadata.get(key) != value for key, value in expected.items()):
        raise ValueError("Package metadata mismatch")
    resources = package / "codex-resources"
    if (
        not resources.resolve().is_relative_to(package.resolve())
        or not resources.is_dir()
    ):
        raise ValueError("Missing package resources")
    voice = resources / "voice"
    has_voice = voice.exists() or voice.is_symlink()
    if has_voice and (
        not voice.resolve().is_relative_to(package.resolve()) or not voice.is_dir()
    ):
        raise ValueError("Invalid voice directory")
    executables = [
        "bin/codex",
        "bin/codex-matrix",
        "bin/codex-code-mode-host",
        "codex-path/rg",
    ]
    if has_voice:
        executables.append("codex-resources/voice/bin/codex-voice-host")
    for relative in executables:
        binary = package / relative
        if (
            not binary.resolve().is_relative_to(package.resolve())
            or not binary.is_file()
            or not os.access(binary, os.X_OK)
        ):
            raise ValueError("Missing package executable")
    for relative in (
        ("codex-resources/voice/manifest.json", "codex-resources/voice/runtime.json")
        if has_voice
        else ()
    ):
        resource = package / relative
        if (
            not resource.resolve().is_relative_to(package.resolve())
            or not resource.is_file()
        ):
            raise ValueError("Missing voice resource")
    subprocess.run(
        ["codesign", "--verify", "--strict", str(package / "bin/codex")],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=20,
    )
    output = subprocess.check_output(
        [str(package / "bin/codex"), "--version"],
        text=True,
        stderr=subprocess.DEVNULL,
        timeout=20,
    ).strip()
    if output != f"codex-cli {version}":
        raise ValueError("CLI version mismatch")


def update(root, fallback):
    releases = root / "releases"
    if releases.is_symlink():
        raise ValueError("Invalid release directory")
    releases.mkdir(exist_ok=True)
    current = root / "current"
    if current.exists() and not current.is_symlink():
        raise ValueError("Current must be a symlink")
    installed = fallback.parent.parent
    if current.is_symlink():
        installed = current.resolve(strict=True)
        if not installed.is_relative_to(releases.resolve()):
            raise ValueError("Current escapes release directory")
    local_version = json.loads((installed / "codex-package.json").read_text())[
        "version"
    ]
    local = version_tuple(local_version)
    try:
        with open_url(API) as response:
            latest = json.loads(response.read(2 * 1024 * 1024))
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return "no-release"
        raise
    tag = latest["tag_name"]
    if (
        latest["draft"]
        or latest["prerelease"]
        or not isinstance(tag, str)
        or not tag.startswith("v")
    ):
        raise ValueError("Latest release is not stable")
    version = tag[1:]
    if version_tuple(version) <= local:
        return "already-current"
    folder = f"codex-matrix-{version}-{TARGET}"
    archive_name = folder + ".tar.gz"
    selected = []
    for name in (archive_name, archive_name + ".sha256"):
        matches = [asset for asset in latest["assets"] if asset["name"] == name]
        if len(matches) != 1:
            raise ValueError("Missing or duplicate release asset")
        selected.append(matches[0])
    with tempfile.TemporaryDirectory(prefix=".update-", dir=root) as temporary:
        work = Path(temporary)
        archive = work / archive_name
        checksum = work / (archive_name + ".sha256")
        download(selected[0], archive)
        download(selected[1], checksum)
        if checksum.read_text() != f"{digest(archive)}  {archive_name}\n":
            raise ValueError("Companion checksum mismatch")
        unpacked = work / "unpacked"
        unpacked.mkdir()
        extract(archive, unpacked, folder)
        package = unpacked / folder
        validate_package(package, version)
        marker = package / ".matrix-archive-sha256"
        if marker.exists() or marker.is_symlink():
            raise ValueError("Unexpected installation marker")
        marker.write_text(digest(archive))
        destination = releases / folder
        if destination.exists() or destination.is_symlink():
            validate_package(destination, version)
            if (destination / marker.name).read_text() != digest(archive):
                raise ValueError("Existing package differs from verified archive")
        else:
            os.rename(package, destination)
        pointer = work / "current"
        pointer.symlink_to(Path("releases") / folder)
        os.replace(pointer, current)
    return f"updated:{version}"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fallback", required=True, type=Path)
    parser.add_argument(
        "--root", type=Path, default=Path.home() / ".codex/packages/matrix"
    )
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)
    result = "unsupported-platform"
    try:
        if platform.system() == "Darwin" and platform.machine() == "arm64":
            root = args.root.expanduser().resolve()
            root.mkdir(parents=True, exist_ok=True)
            lock_fd = os.open(
                root / ".update.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600
            )
            with os.fdopen(lock_fd, "w") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    result = "another-update-running"
                else:
                    try:
                        result = update(root, args.fallback.expanduser().resolve())
                    except Exception as error:
                        result = "update-failed:" + type(error).__name__
                    with tempfile.NamedTemporaryFile(
                        mode="w", prefix=".status-", dir=root, delete=False
                    ) as status:
                        json.dump({"result": result}, status)
                    os.replace(status.name, root / "update-status.json")
    except Exception as error:
        result = "update-failed:" + type(error).__name__
    if args.verbose or result.startswith("updated:"):
        print("Matrix updater: " + result, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
