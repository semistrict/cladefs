# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/firecracker-cargo.sh, which builds Firecracker as cladefs builds it (TASK-269): in
a copy of vendor/firecracker whose vmm manifest has cladefs's crate as a dependency and whose lock is
cladefs's, neither of which the series has. In a repository of its own laid out as this one, with a
cargo that records how it was run."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

VMM_MANIFEST = '[package]\nname = "vmm"\n\n[features]\ncladefs = []\n\n[dependencies]\nlibc = "0.2"\n'


class FirecrackerCargoTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name).resolve() / "cladefs"
        (self.root / "scripts").mkdir(parents=True)
        shutil.copy(SCRIPTS / "firecracker-cargo.sh", self.root / "scripts")
        self.src = self.root / "vendor/firecracker"
        (self.src / "src/vmm").mkdir(parents=True)
        (self.src / "src/vmm/Cargo.toml").write_text(VMM_MANIFEST)
        (self.src / "Cargo.lock").write_text("the series' lock\n")
        (self.src / "README.md").write_text("upstream\n")
        (self.src / ".git").write_text("gitdir: elsewhere\n")
        (self.root / "vendor/firecracker.lock").write_text("cladefs's lock\n")
        self.tree = self.root / "target/firecracker"
        # cargo: each run's directory and words in calls; `metadata --locked` fails if $STALE.
        bin_ = Path(scratch.name) / "bin"
        bin_.mkdir()
        self.calls = Path(scratch.name) / "calls"
        (bin_ / "cargo").write_text(
            '#!/bin/bash\necho "$PWD $*" >> "$CALLS"\n'
            '[[ -z $STALE || "$*" != "metadata --locked"* ]]\n')
        (bin_ / "cargo").chmod(0o755)
        self.env = {**os.environ, "PATH": f"{bin_}:{os.environ['PATH']}",
                    "CALLS": str(self.calls)}
        for name in ["CLADEFS_FIRECRACKER_SRC", "CLADEFS_FIRECRACKER_TREE",
                     "CLADEFS_FIRECRACKER_UNLOCKED"]:
            self.env.pop(name, None)

    def run_script(self, *args, **env):
        return subprocess.run([self.root / "scripts/firecracker-cargo.sh", *args],
                              capture_output=True, text=True, env={**self.env, **env})

    def cargo_calls(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_builds_in_a_copy_with_cladefss_crates_and_lock(self):
        done = self.run_script("build", "-p", "firecracker", "--features", "cladefs")
        self.assertEqual((done.returncode, done.stderr), (0, ""))
        self.assertEqual(
            (self.tree / "src/vmm/Cargo.toml").read_text(),
            VMM_MANIFEST + "\n# cladefs: its crates, which the cladefs feature builds against "
            "(scripts/firecracker-cargo.sh).\n"
            f'[dependencies.cladefs]\npath = "{self.root}/crates/cladefs"\n'
            f'[dependencies.virtio_extent]\npath = "{self.root}/crates/virtio_extent"\n')
        self.assertEqual((self.tree / "Cargo.lock").read_text(), "cladefs's lock\n")
        self.assertEqual((self.tree / "README.md").read_text(), "upstream\n")
        self.assertFalse((self.tree / ".git").exists())
        # The series' own stay as they are.
        self.assertEqual((self.src / "src/vmm/Cargo.toml").read_text(), VMM_MANIFEST)
        self.assertEqual((self.src / "Cargo.lock").read_text(), "the series' lock\n")
        self.assertEqual(self.cargo_calls(), [
            f"{self.tree} metadata --locked --format-version 1",
            f"{self.tree} build -p firecracker --features cladefs",
        ])

    def test_a_copy_again_rewrites_only_what_changed(self):
        self.assertEqual(self.run_script("build").returncode, 0)
        manifest = self.tree / "src/vmm/Cargo.toml"
        os.utime(manifest, (1, 1))
        os.utime(self.tree / "README.md", (1, 1))
        (self.src / "README.md").write_text("changed\n")
        self.assertEqual(self.run_script("build").returncode, 0)
        self.assertEqual(manifest.stat().st_mtime, 1)
        self.assertEqual((self.tree / "README.md").read_text(), "changed\n")
        self.assertNotEqual((self.tree / "README.md").stat().st_mtime, 1)

    def test_a_stale_lock_fails_unless_unlocked(self):
        done = self.run_script("build", STALE="1")
        self.assertEqual(done.returncode, 1)
        self.assertIn("vendor/firecracker.lock is not as Cargo resolves the tree", done.stderr)
        self.assertEqual(self.cargo_calls(), [f"{self.tree} metadata --locked --format-version 1"])
        self.calls.unlink()
        done = self.run_script("build", STALE="1", CLADEFS_FIRECRACKER_UNLOCKED="1")
        self.assertEqual(done.returncode, 0)
        self.assertEqual(self.cargo_calls(), [f"{self.tree} build"])

    def test_lock_writes_the_copys_lock_back(self):
        self.assertEqual(self.run_script("--lock").returncode, 0)
        self.assertEqual(self.cargo_calls(), [f"{self.tree} metadata --format-version 1"])
        (self.tree / "Cargo.lock").write_text("resolved\n")
        # A cargo that resolves the copy's lock anew, as `cargo metadata` does.
        (self.root / "resolved").write_text("resolved\n")
        self.calls.unlink()
        bin_cargo = Path(self.env["PATH"].split(":")[0]) / "cargo"
        bin_cargo.write_text('#!/bin/bash\necho "$PWD $*" >> "$CALLS"\necho resolved > Cargo.lock\n')
        self.assertEqual(self.run_script("--lock").returncode, 0)
        self.assertEqual((self.root / "vendor/firecracker.lock").read_text(), "resolved\n")

    def test_a_copy_outside_the_repo_is_refused(self):
        done = self.run_script("build", CLADEFS_FIRECRACKER_TREE="/tmp/elsewhere")
        self.assertEqual(done.returncode, 2)
        self.assertIn("is not in", done.stderr)
        self.assertEqual(self.cargo_calls(), [])

    def test_tree_prints_the_copy(self):
        done = self.run_script("--tree")
        self.assertEqual((done.returncode, done.stdout), (0, f"{self.tree}\n"))


class FirecrackerLockHookTest(unittest.TestCase):
    """Board 10216: scripts/hooks/pre-commit refuses a commit touching Cargo.lock or a workspace
    manifest while vendor/firecracker.lock is stale or unstaged, and stays fast (no cargo call)
    when no dependency file is staged."""

    HOOK = SCRIPTS / "hooks" / "pre-commit"

    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.repo = Path(scratch.name).resolve() / "cladefs"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "core.hooksPath", str(self.HOOK.parent))
        self.calls = Path(scratch.name) / "hook-calls"
        (self.repo / "scripts").mkdir()
        # Fake firecracker-cargo.sh: records every call and fails --check when $STALE is set.
        script = self.repo / "scripts" / "firecracker-cargo.sh"
        script.write_text(
            "#!/usr/bin/env bash\n"
            'echo "$*" >> "$HOOK_CALLS"\n'
            'if [[ -n ${STALE:-} ]]; then\n'
            '    echo "vendor/firecracker.lock is not as Cargo resolves the tree" >&2\n'
            "    exit 1\n"
            "fi\n"
        )
        script.chmod(0o755)
        self.write("Cargo.toml", "[workspace]\n")
        self.write("Cargo.lock", "v1\n")
        self.write("crates/cladefs/Cargo.toml", '[package]\nname = "cladefs"\n')
        self.write("crates/cladefs/src/lib.rs", "pub fn ok() {}\n")
        self.write("vendor/firecracker.lock", "fc-v1\n")
        self.git("add", "-A")
        self.git("commit", "-q", "--no-verify", "-m", "base")
        self.env = {**os.environ, "HOOK_CALLS": str(self.calls)}
        self.env.pop("STALE", None)

    def git(self, *args):
        return subprocess.run(["git", "-C", self.repo, *args], check=True, capture_output=True,
                              text=True).stdout.strip()

    def write(self, path, text):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)

    def commit(self, message, **env):
        return subprocess.run(
            ["git", "-C", self.repo, "commit", "-q", "-m", message],
            capture_output=True, text=True,
            env={**self.env, **env, "GIT_DIR": str(self.repo / ".git")},
        )

    def hook_calls(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_unrelated_commit_skips_firecracker_check(self):
        self.write("crates/cladefs/src/lib.rs", "pub fn ok() -> u32 { 1 }\n")
        self.git("add", "crates/cladefs/src/lib.rs")
        done = self.commit("touch rust source only")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.hook_calls(), [])

    def test_stale_lock_refuses_manifest_or_cargo_lock_commit(self):
        for path, content in [
            ("Cargo.lock", "v2\n"),
            ("crates/cladefs/Cargo.toml", '[package]\nname = "cladefs"\nversion = "0.2.0"\n'),
        ]:
            with self.subTest(path=path):
                self.write(path, content)
                self.git("add", path)
                done = self.commit(f"update {path}", STALE="1")
                self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
                self.assertIn("vendor/firecracker.lock is not as Cargo resolves the tree",
                              done.stderr)
                self.assertEqual(self.hook_calls(), ["--check"])
                self.calls.unlink()
                self.git("reset", "-q", "HEAD", "--", path)
                self.git("checkout", "--", path)

    def test_unstaged_firecracker_lock_is_refused(self):
        self.write("Cargo.lock", "v2\n")
        self.git("add", "Cargo.lock")
        # Refreshed on disk, forgotten in git add: must be refused before running --check.
        self.write("vendor/firecracker.lock", "fc-v2\n")
        done = self.commit("forgot to stage firecracker.lock")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("vendor/firecracker.lock has unstaged changes", done.stderr)
        self.assertEqual(self.hook_calls(), [])

    def test_fresh_staged_firecracker_lock_is_allowed(self):
        self.write("Cargo.lock", "v2\n")
        self.write("vendor/firecracker.lock", "fc-v2\n")
        self.git("add", "Cargo.lock", "vendor/firecracker.lock")
        done = self.commit("update lockfiles together")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.hook_calls(), ["--check"])


if __name__ == "__main__":
    unittest.main()
