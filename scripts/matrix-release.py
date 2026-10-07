#!/usr/bin/env python3
"""Check, build, and publish the native Matrix Codex distribution."""

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / ".matrix-build/dist"
TARGET = "aarch64-apple-darwin"
UPSTREAM = "openai/codex"


def run(*args, cwd=ROOT):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def version_tuple(value):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise ValueError(f"Invalid stable version: {value!r}")
    return tuple(map(int, value.split(".")))


def api(path, method="GET", data=None, missing=False):
    request = urllib.request.Request(
        "https://api.github.com/" + path,
        data=None if data is None else json.dumps(data).encode(),
        method=method,
        headers={
            "Authorization": "Bearer " + os.environ["GH_TOKEN"],
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if missing and error.code == 404:
            return None
        raise


def own_release(repo, version):
    # The tag endpoint may omit drafts; authenticated listing finds retryable drafts.
    release = api(f"repos/{repo}/releases/tags/v{version}", missing=True)
    if release is not None:
        return release
    page = 1
    while True:
        releases = api(f"repos/{repo}/releases?per_page=100&page={page}")
        for release in releases:
            if release["tag_name"] == f"v{version}":
                return release
        if len(releases) < 100:
            return None
        page += 1


def check():
    base = (ROOT / "matrix-upstream-version.txt").read_text().strip()
    current = version_tuple(base)
    baseline = (ROOT / "matrix-baseline-version.txt").read_text().strip()
    if current < version_tuple(baseline):
        raise ValueError("Current version predates immutable baseline")
    repo = os.environ["GITHUB_REPOSITORY"]
    release = own_release(repo, base)
    if release and release.get("prerelease") and not release["draft"]:
        raise ValueError("Existing published release is marked prerelease")
    latest = api(f"repos/{UPSTREAM}/releases/latest")
    tag = latest["tag_name"]
    if latest["draft"] or latest["prerelease"] or not tag.startswith("rust-v"):
        raise ValueError("Upstream latest is not a stable Rust release")
    version = tag.removeprefix("rust-v")
    candidate = version_tuple(version)
    # Finish publication after a successful source push before advancing again.
    if (
        (release is None and base != baseline)
        or (release and release["draft"])
        or candidate <= current
    ):
        version, tag = base, f"rust-v{base}"
    else:
        release = own_release(repo, version)
    update = (release is None and version != baseline) or bool(
        release and release["draft"]
    )
    if release and release.get("prerelease") and not release["draft"]:
        raise ValueError("Existing published release is marked prerelease")
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(
            f"version={version}\nupstream_tag={tag}\nupdate={str(update).lower()}\n"
        )
    print(f"version={version}, update={update}")


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_official_digest(asset, archive):
    digest = asset.get("digest")
    if not isinstance(digest, str) or not re.fullmatch(
        r"sha256:[0-9a-fA-F]{64}", digest
    ):
        raise ValueError("Official package SHA-256 digest missing or malformed")
    if digest.lower() != "sha256:" + sha256(archive):
        raise ValueError("Official package digest mismatch")


def extract(archive, destination):
    destination.mkdir(parents=True, exist_ok=True)
    root = destination.resolve()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            name = PurePosixPath(member.name)
            if name.is_absolute() or ".." in name.parts:
                raise ValueError("Unsafe archive member")
            target = root / member.name
            if not target.resolve().is_relative_to(root):
                raise ValueError("Archive member escapes destination")
            if member.issym() or member.islnk():
                link = PurePosixPath(member.linkname)
                parent = target.parent if member.issym() else root
                if link.is_absolute() or not (
                    parent / member.linkname
                ).resolve().is_relative_to(root):
                    raise ValueError("Unsafe archive link")
            elif not member.isfile() and not member.isdir():
                raise ValueError("Unsupported archive member")
        tar.extractall(destination, filter="data")


def smoke(binary, version):
    if run(str(binary), "--version") != f"codex-cli {version}":
        raise ValueError("Compiled CLI version mismatch")


def asset_names(version):
    archive = f"codex-matrix-{version}-{TARGET}.tar.gz"
    return [archive, archive + ".sha256"]


def prepare_source(version, base):
    if run("git", "status", "--porcelain", "--untracked-files=no"):
        raise ValueError("Build requires a clean tracked checkout")
    protected = (
        "scripts/matrix-release.py",
        ".github/workflows/matrix-release.yml",
        "matrix-baseline-version.txt",
    )
    digests = {path: sha256(ROOT / path) for path in protected}
    base_commit = run("git", "rev-parse", "HEAD")
    run(
        "git",
        "fetch",
        "--no-tags",
        "https://github.com/openai/codex.git",
        f"refs/tags/rust-v{version}",
    )
    upstream = run("git", "rev-parse", "FETCH_HEAD^{commit}")
    if version_tuple(version) > version_tuple(base):
        try:
            run(
                "git",
                "-c",
                "user.name=github-actions[bot]",
                "-c",
                "user.email=41898282+github-actions[bot]@users.noreply.github.com",
                "merge",
                "--no-ff",
                "--no-commit",
                upstream,
            )
        except subprocess.CalledProcessError:
            conflicts = run(
                "git", "diff", "--name-only", "--diff-filter=U"
            ).splitlines()
            if not conflicts or any(
                not path.startswith(".github/workflows/") for path in conflicts
            ):
                raise
        # Keep only the fork's automation, including after workflow-only conflicts.
        run(
            "git",
            "restore",
            f"--source={base_commit}",
            "--staged",
            "--worktree",
            "--",
            ".github/workflows",
        )
        if run("git", "diff", "--name-only", "--diff-filter=U"):
            raise ValueError("Unresolved merge conflicts")
        if run("git", "rev-parse", "MERGE_HEAD") != upstream:
            raise ValueError("Expected a pending official upstream merge")
        if any(sha256(ROOT / path) != digest for path, digest in digests.items()):
            raise ValueError("Upstream merge changed release automation or baseline")
        (ROOT / "matrix-upstream-version.txt").write_text(version + "\n")
        run("git", "add", "--", "matrix-upstream-version.txt")
        run(
            "git",
            "-c",
            "user.name=github-actions[bot]",
            "-c",
            "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit",
            "-m",
            f"chore: Codex {version} 공식 릴리즈 병합",
        )
    else:
        run("git", "merge-base", "--is-ancestor", upstream, "HEAD")
    return upstream


def build():
    version = os.environ["VERSION"]
    version_tuple(version)
    base = (ROOT / "matrix-upstream-version.txt").read_text().strip()
    version_tuple(base)
    if version_tuple(version) < version_tuple(base):
        raise ValueError("Refusing downgrade")
    if run("uname", "-sm") != "Darwin arm64":
        raise ValueError("Native Apple Silicon runner required")
    work = ROOT / ".matrix-build"
    work.mkdir(exist_ok=True)
    base_commit = run("git", "rev-parse", "HEAD")
    if base_commit != os.environ["GITHUB_SHA"]:
        raise ValueError("Build checkout differs from event commit")
    source = ROOT
    upstream_commit = prepare_source(version, base)
    commit = run("git", "rev-parse", "HEAD")
    # Stream compiler output; do not buffer a multi-hour build in memory.
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "-p",
            "codex-cli",
            "--bin",
            "codex",
        ],
        cwd=source / "codex-rs",
        check=True,
        env={**os.environ, "CARGO_BUILD_JOBS": "2", "CARGO_INCREMENTAL": "0"},
    )
    binary = source / "codex-rs/target/release/codex"
    smoke(binary, version)
    release = api(f"repos/{UPSTREAM}/releases/tags/rust-v{version}")
    if release["draft"] or release["prerelease"]:
        raise ValueError("Unstable package release")
    assets = [
        a for a in release["assets"] if a["name"] == f"codex-package-{TARGET}.tar.gz"
    ]
    if len(assets) != 1:
        raise ValueError("Expected one official full package")
    asset = assets[0]
    archive = work / "official.tar.gz"
    with (
        urllib.request.urlopen(asset["browser_download_url"], timeout=120) as response,
        archive.open("wb") as output,
    ):
        shutil.copyfileobj(response, output)
    if archive.stat().st_size != asset["size"]:
        raise ValueError("Official package size mismatch")
    verify_official_digest(asset, archive)
    unpacked = work / "package"
    extract(archive, unpacked)
    manifests = list(unpacked.rglob("codex-package.json"))
    if len(manifests) != 1:
        raise ValueError("Expected one package manifest")
    package = manifests[0].parent
    metadata = json.loads(manifests[0].read_text())
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
        raise ValueError("Official package metadata mismatch")
    for directory in ("codex-resources", "codex-path"):
        if not (package / directory).is_dir():
            raise ValueError("Official package resources missing")
    helper_modes = {
        p.relative_to(package): p.stat().st_mode & 0o111
        for p in package.rglob("*")
        if p.is_file() and p.stat().st_mode & 0o111
    }
    shutil.copy2(binary, package / "bin/codex")
    run("codesign", "--force", "--sign", "-", str(package / "bin/codex"))
    run("codesign", "--verify", "--strict", str(package / "bin/codex"))
    wrapper = package / "bin/codex-matrix"
    wrapper.write_text(
        '#!/bin/sh\nexport CODEX_MATRIX_RAIN=1\nexec "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/codex" "$@"\n'
    )
    wrapper.chmod(0o755)
    for relative, mode in helper_modes.items():
        if (package / relative).stat().st_mode & 0o111 != mode:
            raise ValueError("Package helper executable mode changed")
    for name in ("LICENSE", "NOTICE"):
        shutil.copy2(ROOT / name, package / name)
    smoke(wrapper, version)
    DIST.mkdir(parents=True, exist_ok=True)
    names = asset_names(version)
    output = DIST / names[0]
    with tarfile.open(output, "w:gz") as tar:
        tar.add(package, arcname=f"codex-matrix-{version}-{TARGET}")
    (DIST / names[1]).write_text(f"{sha256(output)}  {names[0]}\n")
    bundle = DIST / "source.bundle"
    if commit != base_commit:
        run("git", "bundle", "create", str(bundle), f"{base_commit}..HEAD")
    (DIST / "build-source.json").write_text(
        json.dumps(
            {
                "base_commit": base_commit,
                "source_commit": commit,
                "base_version": base,
                "version": version,
                "upstream_commit": upstream_commit,
                "bundle_sha256": sha256(bundle) if commit != base_commit else None,
                "asset_sha256": {name: sha256(DIST / name) for name in names},
            },
            indent=2,
        )
        + "\n"
    )
    (DIST / "release-notes.md").write_text(
        f"OpenAI Codex {version}에 Matrix 배경 효과를 적용한 macOS Apple Silicon 배포.\n\n"
        "압축 해제 후 `bin/codex-matrix` 실행. 공식 helper 및 음성 리소스 포함.\n"
        "네이티브 release 빌드, 버전 확인, ad hoc 서명 검증 완료. UI 테스트는 실행하지 않음.\n"
    )
    verification = work / "archive-verification"
    extract(output, verification)
    smoke(verification / f"codex-matrix-{version}-{TARGET}/bin/codex-matrix", version)


def verify_dist(version):
    version_tuple(version)
    for name in asset_names(version) + ["release-notes.md", "build-source.json"]:
        if not (DIST / name).is_file() or (DIST / name).stat().st_size == 0:
            raise ValueError(f"Missing artifact: {name}")
    archive, checksum = asset_names(version)
    if (DIST / checksum).read_text() != f"{sha256(DIST / archive)}  {archive}\n":
        raise ValueError("Archive checksum mismatch")
    provenance = json.loads((DIST / "build-source.json").read_text())
    base = (ROOT / "matrix-upstream-version.txt").read_text().strip()
    if (
        provenance["base_commit"] != run("git", "rev-parse", "HEAD")
        or provenance["base_commit"] != os.environ["GITHUB_SHA"]
        or provenance["base_version"] != base
        or provenance["version"] != version
        or version_tuple(version) < version_tuple(base)
    ):
        raise ValueError("Artifact source provenance mismatch")
    for key in ("base_commit", "source_commit", "upstream_commit"):
        if not re.fullmatch(r"[0-9a-f]{40}", provenance[key]):
            raise ValueError("Invalid source commit")
    for name in asset_names(version):
        if provenance["asset_sha256"].get(name) != sha256(DIST / name):
            raise ValueError("Artifact digest mismatch")
    bundle = DIST / "source.bundle"
    if version != base:
        if (
            provenance["source_commit"] == provenance["base_commit"]
            or not bundle.is_file()
        ):
            raise ValueError("Updated version needs a source bundle")
        if provenance["bundle_sha256"] != sha256(bundle):
            raise ValueError("Source bundle digest mismatch")
        run("git", "bundle", "verify", str(bundle))
        run("git", "fetch", "--no-tags", str(bundle), "HEAD")
        if run("git", "rev-parse", "FETCH_HEAD") != provenance["source_commit"]:
            raise ValueError("Bundle source commit mismatch")
        parents = run(
            "git", "show", "-s", "--format=%P", provenance["source_commit"]
        ).split()
        if parents != [provenance["base_commit"], provenance["upstream_commit"]]:
            raise ValueError("Bundle is not the expected upstream merge")
    elif (
        provenance["source_commit"] != provenance["base_commit"]
        or bundle.exists()
        or provenance["bundle_sha256"] is not None
    ):
        raise ValueError("Retry must use the unchanged source commit")
    run(
        "git",
        "fetch",
        "--no-tags",
        "https://github.com/openai/codex.git",
        f"refs/tags/rust-v{version}",
    )
    if run("git", "rev-parse", "FETCH_HEAD^{commit}") != provenance["upstream_commit"]:
        raise ValueError("Built upstream commit differs from official tag")
    run(
        "git",
        "merge-base",
        "--is-ancestor",
        provenance["upstream_commit"],
        provenance["source_commit"],
    )
    if (
        run("git", "show", f"{provenance['source_commit']}:matrix-upstream-version.txt")
        != version
    ):
        raise ValueError("Built source version mismatch")
    for path in (
        "scripts/matrix-release.py",
        ".github/workflows",
        "matrix-baseline-version.txt",
    ):
        if run(
            "git",
            "diff",
            provenance["base_commit"],
            provenance["source_commit"],
            "--",
            path,
        ):
            raise ValueError("Release automation or baseline changed during merge")
    return provenance


def publish():
    version = os.environ["VERSION"]
    provenance = verify_dist(version)
    repo = os.environ["GITHUB_REPOSITORY"]
    branch = os.environ["DEFAULT_BRANCH"]
    release = own_release(repo, version)
    if release and not release["draft"]:
        raise ValueError("Refusing to modify a published release")
    commit = provenance["source_commit"]
    remote = run(
        "git", "ls-remote", "--heads", "origin", f"refs/heads/{branch}"
    ).split()
    if not remote or remote[0] != provenance["base_commit"]:
        raise ValueError("Remote branch changed since this workflow started")
    run("git", "merge", "--ff-only", commit)
    if run("git", "rev-parse", "HEAD") != commit:
        raise ValueError("Publication checkout differs from built source")
    tag = f"v{version}"
    existing = api(f"repos/{repo}/git/ref/tags/{tag}", missing=True)
    if existing and (
        existing["object"]["type"] != "commit" or existing["object"]["sha"] != commit
    ):
        raise ValueError("Existing release tag does not match built source commit")
    if release and release["target_commitish"] != commit:
        raise ValueError("Draft release target mismatch")
    run("git", "push", "origin", f"HEAD:refs/heads/{branch}")
    if release is None:
        release = api(
            f"repos/{repo}/releases",
            "POST",
            {
                "tag_name": tag,
                "target_commitish": commit,
                "name": f"Codex Matrix {version}",
                "body": (DIST / "release-notes.md").read_text(),
                "draft": True,
                "prerelease": False,
            },
        )
    names = asset_names(version)
    run(
        "gh",
        "release",
        "upload",
        tag,
        *[str(DIST / name) for name in names],
        "--clobber",
        "--repo",
        repo,
    )
    uploaded = api(f"repos/{repo}/releases/{release['id']}")
    by_name = {asset["name"]: asset for asset in uploaded["assets"]}
    for name in names:
        asset = by_name.get(name)
        if (
            not asset
            or asset.get("state") != "uploaded"
            or asset["size"] != (DIST / name).stat().st_size
        ):
            raise ValueError(f"Incomplete release asset: {name}")
        if asset.get("digest") != "sha256:" + sha256(DIST / name):
            raise ValueError(f"Release asset digest mismatch: {name}")
    api(
        f"repos/{repo}/releases/{release['id']}",
        "PATCH",
        {"draft": False, "prerelease": False, "make_latest": "true"},
    )


if __name__ == "__main__":
    {"check": check, "build": build, "publish": publish}[sys.argv[1]]()
