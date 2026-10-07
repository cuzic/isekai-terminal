#!/usr/bin/env python3
"""Unit tests for cargo_check_on_edit.py (no cargo is executed).

Run: python3 .claude/hooks/test_cargo_check_on_edit.py
"""
from __future__ import annotations

import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cargo_check_on_edit as hook  # noqa: E402


def write(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


class RepoFixture:
    """A miniature repo layout mirroring rust-core's workspace shape."""

    def __init__(self, root: Path) -> None:
        self.root = root
        (root / ".git").mkdir(parents=True)
        self.workspace = write(
            root / "rust-core/Cargo.toml",
            '[workspace]\nmembers = [".", "member"]\n\n[package]\nname = "root-crate"\n',
        )
        self.root_src = write(root / "rust-core/src/lib.rs", "")
        self.member = write(root / "rust-core/member/Cargo.toml", '[package]\nname = "member"\n')
        self.member_src = write(root / "rust-core/member/src/lib.rs", "")
        # Independent workspace root that is NOT a member of rust-core/Cargo.toml
        # (same shape as rust-core/noq-multipath-spike).
        self.spike = write(
            root / "rust-core/spike/Cargo.toml",
            '[package]\nname = "spike"\n\n[workspace]\n',
        )
        self.spike_src = write(root / "rust-core/spike/src/main.rs", "")


class FindWorkspaceManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = RepoFixture(Path(self._tmp.name))

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_member_crate_resolves_to_parent_workspace(self) -> None:
        got = hook.find_workspace_manifest(self.repo.member, self.repo.root)
        self.assertEqual(got, self.repo.workspace)

    def test_root_crate_resolves_to_its_own_workspace_manifest(self) -> None:
        got = hook.find_workspace_manifest(self.repo.workspace, self.repo.root)
        self.assertEqual(got, self.repo.workspace)

    def test_independent_workspace_root_resolves_to_itself(self) -> None:
        got = hook.find_workspace_manifest(self.repo.spike, self.repo.root)
        self.assertEqual(got, self.repo.spike)

    def test_no_workspace_anywhere_falls_back_to_crate_manifest(self) -> None:
        lone = write(self.repo.root / "other/lone/Cargo.toml", '[package]\nname = "lone"\n')
        got = hook.find_workspace_manifest(lone, self.repo.root)
        self.assertEqual(got, lone)

    def test_workspace_dependencies_table_counts_as_workspace(self) -> None:
        self.repo.workspace.write_text('[workspace.dependencies]\nfoo = "1"\n')
        self.assertTrue(hook.has_workspace_table(self.repo.workspace))
        self.assertFalse(hook.has_workspace_table(self.repo.member))


def cargo_result(returncode: int, diagnostics: list[str]) -> subprocess.CompletedProcess:
    lines = []
    for text in diagnostics:
        lines.append(json.dumps({
            "reason": "compiler-message",
            "message": {
                "level": "warning",
                "message": text,
                "rendered": f"warning: {text}",
                "spans": [{"file_name": "src/lib.rs"}],
            },
        }))
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout="\n".join(lines), stderr="")


class MainTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = RepoFixture(Path(self._tmp.name) / "repo")
        self._cache = tempfile.TemporaryDirectory()
        patcher = mock.patch.object(hook, "CACHE_DIR", Path(self._cache.name))
        patcher.start()
        self.addCleanup(patcher.stop)

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._cache.cleanup()

    def run_hook(self, edited: Path, result: subprocess.CompletedProcess) -> tuple[int, str, list]:
        payload = json.dumps({"tool_input": {"file_path": str(edited)}})
        calls: list = []

        def fake_build(workspace_manifest: Path, crate_name: str) -> subprocess.CompletedProcess:
            calls.append((workspace_manifest, crate_name))
            return result

        out = io.StringIO()
        with mock.patch.object(hook, "run_cargo_build", fake_build), \
                mock.patch("sys.stdin", io.StringIO(payload)), \
                mock.patch("sys.stdout", out):
            rc = hook.main()
        return rc, out.getvalue(), calls

    def test_spike_edit_builds_against_its_own_workspace(self) -> None:
        rc, _, calls = self.run_hook(self.repo.spike_src, cargo_result(0, []))
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [(self.repo.spike, "spike")])

    def test_member_edit_builds_against_rust_core_workspace(self) -> None:
        _, _, calls = self.run_hook(self.repo.member_src, cargo_result(0, []))
        self.assertEqual(calls, [(self.repo.workspace, "member")])

    def test_warning_that_disappears_and_returns_is_reported_again(self) -> None:
        rc, out, _ = self.run_hook(self.repo.member_src, cargo_result(0, ["unused variable"]))
        self.assertEqual(rc, 2)
        self.assertIn("unused variable", out)

        rc, _, _ = self.run_hook(self.repo.member_src, cargo_result(0, ["unused variable"]))
        self.assertEqual(rc, 0, "an already-known warning must not be re-reported")

        rc, _, _ = self.run_hook(self.repo.member_src, cargo_result(0, []))
        self.assertEqual(rc, 0)

        rc, out, _ = self.run_hook(self.repo.member_src, cargo_result(0, ["unused variable"]))
        self.assertEqual(rc, 2, "a warning that came back after a clean build is new again")
        self.assertIn("unused variable", out)


if __name__ == "__main__":
    unittest.main()
