"""Real Git regressions for the narrow release version conflict policy."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("matrix_release", Path(__file__).with_name("matrix-release.py"))
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)


class MergeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        for name in ("scripts/matrix-release.py", "scripts/package-matrix-macos.py", "scripts/install-matrix-macos.sh", ".github/workflows/matrix-release.yml", "matrix-baseline-version.txt"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("protected\n")
        (self.root / "matrix-upstream-version.txt").write_text("0.161.0\n")
        (self.root / "codex-rs").mkdir()
        self.manifest = self.root / "codex-rs/Cargo.toml"
        self.manifest.write_text('[workspace.package]\nversion = "0.160.0"\nedition = "2024"\n\n[workspace.dependencies]\nbase = "1"\n')
        (self.root / "codex-rs/Cargo.lock").write_text("version = 4\n")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.git("branch", "upstream")

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True, stderr=subprocess.STDOUT).strip()

    def branches(self, extra_conflict=False, target="0.162.1", diff3=False, workflow=False, base_version="0.160.0"):
        self.manifest.write_text(self.manifest.read_text().replace(base_version, "0.161.0") + '\n# local customization\n')
        if extra_conflict:
            self.manifest.write_text(self.manifest.read_text().replace('edition = "2024"', 'edition = "2021"'))
        if workflow:
            (self.root / ".github/workflows/matrix-release.yml").write_text("local workflow\n")
        self.git("commit", "-qam", "local")
        self.git("checkout", "-q", "upstream")
        self.manifest.write_text(self.manifest.read_text().replace(base_version, target))
        if extra_conflict:
            self.manifest.write_text(self.manifest.read_text().replace('edition = "2024"', 'edition = "2018"'))
        if workflow:
            (self.root / ".github/workflows/matrix-release.yml").write_text("upstream workflow\n")
        self.git("commit", "-qam", "upstream")
        self.git("tag", "rust-v0.162.1")
        self.git("checkout", "-q", "main")
        if diff3:
            self.git("config", "merge.conflictStyle", "diff3")

    def prepare(self):
        original = RELEASE.run
        def local_run(*args, **kwargs):
            args = tuple("." if value == "https://github.com/openai/codex.git" else value for value in args)
            return original(*args, cwd=self.root)
        with patch.object(RELEASE, "ROOT", self.root), patch.object(RELEASE, "run", local_run):
            return RELEASE.prepare_source("0.162.1", "0.161.0")

    def test_version_only_merge_preserves_local_content_and_workflow(self):
        self.branches(workflow=True)
        try:
            self.prepare()
        except subprocess.CalledProcessError as error:
            self.fail(f"Version-only workspace conflict was not resolved: {error}")
        self.assertIn('version = "0.162.1"', self.manifest.read_text())
        self.assertIn("# local customization", self.manifest.read_text())
        self.assertEqual((self.root / ".github/workflows/matrix-release.yml").read_text(), "local workflow\n")
        self.assertEqual(self.git("diff", "--name-only", "--diff-filter=U"), "")
        self.assertEqual(len(self.git("rev-list", "--parents", "-n", "1", "HEAD").split()), 3)

    def test_nonversion_cargo_conflict_stops_and_reports_paths_and_merge_output(self):
        self.branches(extra_conflict=True)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises((subprocess.CalledProcessError, ValueError)):
            self.prepare()
        self.assertIn("codex-rs/Cargo.toml", stderr.getvalue())
        self.assertIn("CONFLICT", stderr.getvalue())
        self.assertEqual(self.git("diff", "--name-only", "--diff-filter=U"), "codex-rs/Cargo.toml")

    def test_workflow_only_conflict_keeps_existing_local_policy(self):
        self.manifest.write_text(self.manifest.read_text().replace("0.160.0", "0.161.0"))
        self.git("commit", "-qam", "shared version")
        self.git("branch", "-f", "upstream", "HEAD")
        self.branches(workflow=True, base_version="0.161.0")
        self.prepare()
        self.assertEqual((self.root / ".github/workflows/matrix-release.yml").read_text(), "local workflow\n")
        self.assertIn('version = "0.162.1"', self.manifest.read_text())

    def test_requested_version_must_match_upstream_manifest(self):
        self.branches(target="0.162.0")
        with self.assertRaises((subprocess.CalledProcessError, ValueError)):
            self.prepare()
        self.assertIn("<<<<<<<", self.manifest.read_text())

    def test_workspace_lock_versions_follow_manifest_without_updating_registry(self):
        self.manifest.write_text('[workspace]\nmembers = ["cli", "fixed"]\n\n' + self.manifest.read_text())
        for name, package in (("cli", 'name = "codex-cli"\nversion.workspace = true'), ("fixed", 'name = "fixed"\nversion = "1.0.0"')):
            folder = self.root / "codex-rs" / name
            folder.mkdir()
            (folder / "Cargo.toml").write_text("[package]\n" + package + "\n")
        lock = self.root / "codex-rs/Cargo.lock"
        lock.write_text('version = 4\n\n[[package]]\nname = "codex-cli"\nversion = "0.161.0"\n\n[[package]]\nname = "fixed"\nversion = "1.0.0"\ndependencies = ["codex-cli 0.161.0", "codex-cli 0.161.0 (registry+https://example.invalid/index)"]\n\n[[package]]\nname = "codex-cli"\nversion = "0.161.0"\nsource = "registry+https://example.invalid/index"\nchecksum = "keep"\n')
        self.git("add", ".")
        self.git("commit", "-qm", "workspace metadata")
        # Both branches share metadata; only release versions diverge.
        self.git("branch", "-f", "upstream", "HEAD")
        self.branches()
        self.prepare()
        import tomllib
        packages = tomllib.loads(lock.read_text())["package"]
        self.assertEqual(packages[0]["version"], "0.162.1")
        self.assertEqual(packages[1]["version"], "1.0.0")
        self.assertEqual(packages[1]["dependencies"], ["codex-cli 0.162.1", "codex-cli 0.161.0 (registry+https://example.invalid/index)"])
        self.assertEqual(packages[2]["version"], "0.161.0")
        self.assertEqual(packages[2]["checksum"], "keep")
        self.assertEqual(self.git("status", "--porcelain", "--untracked-files=no"), "")

    def test_diff3_conflict_is_safely_rejected(self):
        self.branches(diff3=True)
        with self.assertRaises((subprocess.CalledProcessError, ValueError)):
            self.prepare()
        self.assertIn("|||||||", self.manifest.read_text())


if __name__ == "__main__":
    unittest.main()
