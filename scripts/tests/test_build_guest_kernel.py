# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/build-guest-kernel.sh's checks of the Rust it links into the kernel: that no
member of the Rust's archive defines the kernel's string functions, and that the object built from
it does not either; and of its check, and guest/sim/kernel-tree.sh's, that the release's tag names
the commit scripts/lib/guest-kernel-release.sh pins (board 8601). The script runs as it is, in a
tree of its own beside the repository's guest/, with tools in place of those that build: git,
which clones, says what the tag names (FAKE/tag-commit, nothing if empty), makes the series'
commits and the patches;
make, which configures and builds the kernel; cargo, which builds the Rust; nm, which lists what
the archive's members and the object define, as the test says; ar, which records the members it
deletes; nproc; and scripts/build-guest-sysroot.sh."""

import re
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

FUNCTIONS = ["memcpy", "memmove", "memset", "memcmp", "bcmp", "strlen"]
RELEASE = SCRIPTS / "lib" / "guest-kernel-release.sh"
# The commit the release's tag must name, and one it might be moved to.
PINNED = re.search(r"^kernel_commit=([0-9a-f]{40})$", RELEASE.read_text(), re.MULTILINE).group(1)
MOVED = "f" * 40
TAG = re.search(r"^kernel_tag=(\S+)$", RELEASE.read_text(), re.MULTILINE).group(1)
TOOLS = {
    # A clone with fs/cladefs, the series' patches, the commit the tag names, and a commit's, a
    # tree's or a blob's name.
    "git": """\
if [[ $1 == -C ]]; then shift 2; fi
case $1 in
    clone) echo "$@" >> "$FAKE/clones"; mkdir -p "${@: -1}/fs/cladefs" ;;
    format-patch) while [[ $1 != -o ]]; do shift; done; mkdir -p "$2" ;;
    rev-parse)
        if [[ ${@: -1} == *'^{commit}' ]]; then
            [[ -s $FAKE/tag-commit ]] || exit 1
            cat "$FAKE/tag-commit"
        else
            echo 0123456789abcdef0123456789abcdef01234567
        fi
        ;;
    hash-object | write-tree) echo 0123456789abcdef0123456789abcdef01234567 ;;
    commit-tree) cat > /dev/null; echo 0123456789abcdef0123456789abcdef01234567 ;;
esac
""",
    # The kernel's image, and the object of fs/cladefs's Rust.
    "make": """\
src=$2
echo "$@" >> "$FAKE/make"
if [[ ${@: -1} == vmlinux ]]; then
    : > "$src/vmlinux"
    : > "$src/fs/cladefs/cladefs_rust.o"
fi
""",
    "cargo": """\
echo "$CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS $CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS" >> "$FAKE/cargo"
mkdir -p "$CARGO_TARGET_DIR/x86_64-unknown-none/release"
: > "$CARGO_TARGET_DIR/x86_64-unknown-none/release/libcladefs_linux.a"
""",
    # What the archive's members define, by member, or what the object does, as the test says,
    # by one process, as nm's: a reader that stops early ends it.
    "nm": """\
if [[ $1 == -A ]]; then
    while read -r member symbol; do
        echo "$4:$member:0000000000000000 T $symbol"
    done < "$FAKE/archive"
    exit 0
fi
status=$(cat "$FAKE/status")
if (( status == 0 )); then exec cat "$FAKE/object"; fi
cat "$FAKE/object"
exit "$status"
""",
    "ar": 'echo "$3" >> "$FAKE/deleted"\n',
    "nproc": "echo 8\n",
}
MESSAGE = "cladefs_rust.o defines the string functions above, which must be the kernel's\n"


class RustInTheKernel(unittest.TestCase):
    def build(self, object_, archive="", status=0, env=None, tag_commit=PINNED, cloned=False):
        """Builds the x86-64 kernel, nm listing `archive` (a member and a symbol it defines, a line
        each) for the Rust's archive, and `object_`, then ending with `status`, for its object,
        with `env` in its environment too, the release's tag naming `tag_commit`, its sources
        cloned before if `cloned`: the script's status, output and errors."""
        self.directory = Path(tempfile.mkdtemp())
        fake, tree, bin_ = self.directory / "fake", self.directory / "tree", self.directory / "bin"
        for directory in [fake, tree / "scripts" / "lib", bin_]:
            directory.mkdir(parents=True)
        for name, text in {"archive": archive, "object": object_, "status": str(status),
                           "tag-commit": tag_commit}.items():
            (fake / name).write_text(text)
        for name in ["deleted", "make", "clones"]:
            (fake / name).write_text("")
        (tree / "guest").symlink_to(SCRIPTS.parent / "guest")
        for name in ["build-guest-kernel.sh", "build-commit.sh", "lib/guest-kernel-release.sh"]:
            (tree / "scripts" / name).symlink_to(SCRIPTS / name)
        tools = {bin_ / name: script for name, script in TOOLS.items()}
        tools[tree / "scripts" / "build-guest-sysroot.sh"] = "echo -Cpanic=abort\n"
        for path, script in tools.items():
            path.write_text("#!/bin/bash\n" + script)
            path.chmod(0o755)
        self.work = self.directory / "work"
        if cloned:
            (self.work / f"{TAG}-x86_64" / "fs" / "cladefs").mkdir(parents=True)
        done = subprocess.run(
            ["/bin/bash", tree / "scripts" / "build-guest-kernel.sh", "x86_64"],
            env={
                "PATH": f"{bin_}:/usr/bin:/bin",
                "HOME": str(self.directory),
                "FAKE": str(fake),
                "CLADEFS_GUEST_KERNEL_WORK": str(self.work),
                **(env or {}),
            },
            capture_output=True,
            text=True,
        )
        return done.returncode, done.stdout, done.stderr

    def built(self):
        return (self.work / "out" / "x86_64" / "vmlinux").exists()

    def test_a_kernel_whose_rust_uses_the_kernels_string_functions_is_built(self):
        self.assertEqual(
            self.build("                 U memcpy\n0000000000000000 T cladefs_init\n"),
            (0, f"{self.work / 'out' / 'x86_64' / 'vmlinux'}\n", ""),
        )
        self.assertTrue(self.built())

    def test_the_kernel_is_built_with_the_commit_the_tree_is_of(self):
        """TASK-111.7: fs/cladefs shows it in /proc/fs/cladefs/build (scripts/build-commit.sh)."""
        self.build("", env={"CLADEFS_BUILD_COMMIT": "0123abcd-dirty"})
        builds = [line for line in (self.directory / "fake" / "make").read_text().splitlines()
                  if line.endswith(" vmlinux")]
        self.assertEqual(len(builds), 1, builds)
        self.assertIn(" CLADEFS_BUILD_COMMIT=0123abcd-dirty vmlinux", builds[0])

    def test_the_kernel_is_built_with_as_many_jobs_as_asked(self):
        """TASK-276.6: CLADEFS_GUEST_KERNEL_JOBS, on a host others share; the CPUs otherwise."""
        for env, jobs in [({}, "-j8"), ({"CLADEFS_GUEST_KERNEL_JOBS": "4"}, "-j4")]:
            with self.subTest(env):
                self.build("", env=env)
                builds = [line for line in (self.directory / "fake" / "make").read_text()
                          .splitlines() if line.endswith(" vmlinux")]
                self.assertEqual(len(builds), 1, builds)
                self.assertIn(f" {jobs} ", builds[0])

    def test_rust_checks_build_with_debug_assertions_and_overflow_checks(self):
        """TASK-149.1: CLADEFS_GUEST_KERNEL_RUST_CHECKS=1 turns both on, which are off otherwise."""
        for checks, profile in [({}, "false false"),
                                ({"CLADEFS_GUEST_KERNEL_RUST_CHECKS": "1"}, "true true")]:
            with self.subTest(checks):
                self.assertEqual(self.build("", env=checks)[0], 0)
                self.assertEqual((self.directory / "fake" / "cargo").read_text(), profile + "\n")

    def test_the_archive_loses_the_members_that_define_string_functions(self):
        archive = "".join(f"compiler_builtins-{i}.o {name}\n" for i, name in enumerate(FUNCTIONS))
        self.build("", archive + "cladefs_linux-0.o cladefs_init\ncompiler_builtins-6.o memcpy_fast\n")
        self.assertEqual(
            (self.directory / "fake" / "deleted").read_text().split(),
            [f"compiler_builtins-{i}.o" for i in range(len(FUNCTIONS))],
        )

    def test_rust_defining_a_string_function_fails_the_build(self):
        for name in FUNCTIONS:
            with self.subTest(name):
                definition = f"0000000000000000 T {name}\n"
                self.assertEqual(self.build(definition), (1, "", definition + MESSAGE))
                self.assertFalse(self.built())

    def test_a_definition_amid_much_else_fails_the_build(self):
        # More of nm's listing after it than a pipe holds.
        definition = "0000000000000000 W memset\n"
        status, _, errors = self.build(definition + "0000000000000000 T fxfs_other\n" * 100000)
        self.assertEqual((status, errors), (1, definition + MESSAGE))
        self.assertFalse(self.built())

    def test_nm_failing_fails_the_build(self):
        self.assertEqual(self.build("", status=1), (1, "", ""))
        self.assertFalse(self.built())

    def test_sources_whose_tag_names_another_commit_are_refused(self):
        """Board 8601: a tag moved, or a tree kept from before that has it otherwise, is never
        built, whether cloned now or before."""
        for cloned in [False, True]:
            for tag_commit, named in [(MOVED, MOVED), ("", "no commit")]:
                with self.subTest(cloned=cloned, named=named):
                    status, output, errors = self.build("", tag_commit=tag_commit, cloned=cloned)
                    src = self.work / f"{TAG}-x86_64"
                    self.assertEqual((status, output, errors), (1, "", (
                        f"{src}: {TAG} names {named}, not {PINNED}: refused "
                        "(scripts/lib/guest-kernel-release.sh)\n")))
                    fake = self.directory / "fake"
                    self.assertEqual(len((fake / "clones").read_text().splitlines()),
                                     0 if cloned else 1)
                    self.assertEqual((fake / "make").read_text(), "")
                    self.assertFalse(self.built())

    def test_kept_sources_whose_tag_names_the_release_are_built_again(self):
        self.assertEqual(self.build("", cloned=True)[0], 0)
        self.assertEqual((self.directory / "fake" / "clones").read_text(), "")
        self.assertTrue(self.built())

    def test_the_kernel_is_stamped_with_the_releases_date_not_the_builds(self):
        """Board 8601: the version banner says KBUILD_BUILD_TIMESTAMP, the build's time unless
        set, and two builds of one commit differed by it."""
        self.build("")
        builds = [line for line in (self.directory / "fake" / "make").read_text().splitlines()
                  if line.endswith(" vmlinux")]
        self.assertEqual(len(builds), 1, builds)
        self.assertIn(" KBUILD_BUILD_TIMESTAMP=2026-10-17T00:00:00Z ", builds[0])


class SimKernelTree(unittest.TestCase):
    """guest/sim/kernel-tree.sh's tree of the release's sources shares the check (board 8601):
    refused when the tag names another commit, fetched now or kept, marked done, from before. Run as
    it is, with git a fake: a clone makes TREE, rev-parse says FAKE/tag-commit, the rest nothing."""

    GIT = """#!/bin/bash
if [[ $1 == -C ]]; then shift 2; fi
case $1 in
    clone) echo "$@" >> "$FAKE/clones"; mkdir -p "${@: -1}" ;;
    rev-parse) [[ -s $FAKE/tag-commit ]] || exit 1; cat "$FAKE/tag-commit" ;;
esac
"""

    def tree(self, tag_commit, kept):
        directory = Path(tempfile.mkdtemp())
        fake, bin_, tree = directory / "fake", directory / "bin", directory / "tree"
        fake.mkdir()
        bin_.mkdir()
        (fake / "tag-commit").write_text(tag_commit)
        (fake / "clones").write_text("")
        (bin_ / "git").write_text(self.GIT)
        (bin_ / "git").chmod(0o755)
        if kept:
            tree.mkdir()
            (tree / f".sim-tree-{TAG}").write_text("")
        done = subprocess.run(
            ["/bin/bash", SCRIPTS.parent / "guest" / "sim" / "kernel-tree.sh", tree],
            env={"PATH": f"{bin_}:/usr/bin:/bin", "FAKE": str(fake)},
            capture_output=True, text=True)
        clones = len((fake / "clones").read_text().splitlines())
        marked = (tree / f".sim-tree-{TAG}").exists()
        return done.returncode, done.stderr.replace(str(tree), "TREE"), clones, marked

    def test_a_tree_whose_tag_names_another_commit_is_refused(self):
        refusal = f"TREE: {TAG} names {MOVED}, not {PINNED}: refused (scripts/lib/guest-kernel-release.sh)\n"
        self.assertEqual(self.tree(MOVED, kept=False), (1, refusal, 1, False))
        self.assertEqual(self.tree(MOVED, kept=True), (1, refusal, 0, True))

    def test_a_tree_of_the_release_is_made_or_kept(self):
        self.assertEqual(self.tree(PINNED, kept=False), (0, "", 1, True))
        self.assertEqual(self.tree(PINNED, kept=True), (0, "", 0, True))


class GuestKernelPatchIndex(unittest.TestCase):
    """guest/linux/patches/README.md indexes every guest kernel patch with its origin and stays in
    sync with the patch files on disk, scripts/build-guest-kernel.sh, and
    tools/bazel/guest_kernel.bzl."""

    ORIGINS = {"project", "cladefs-generic", "upstream"}

    def test_patch_index_matches_disk_and_build_lists(self):
        root = SCRIPTS.parent
        patches_dir = root / "guest" / "linux" / "patches"
        on_disk = sorted(p.name for p in patches_dir.glob("*.patch"))

        readme_rows = re.findall(
            r"^\|\s*`([^`]+\.patch)`\s*\|\s*`([^`]+)`\s*\|",
            (patches_dir / "README.md").read_text(),
            re.MULTILINE,
        )
        indexed = [name for name, _ in readme_rows]
        self.assertEqual(indexed, on_disk)
        for name, origin in readme_rows:
            self.assertIn(origin, self.ORIGINS, f"{name}: unexpected origin {origin!r}")

        build_sh = re.findall(
            r"^commit\s+(\S+\.patch)\b",
            (SCRIPTS / "build-guest-kernel.sh").read_text(),
            re.MULTILINE,
        )
        self.assertEqual(build_sh, indexed)

        bzl_text = (root / "tools" / "bazel" / "guest_kernel.bzl").read_text()
        bzl_block = re.search(r"for patch in \\(.*?); do", bzl_text, re.DOTALL)
        self.assertIsNotNone(bzl_block, "patch loop not found in tools/bazel/guest_kernel.bzl")
        bzl_patches = re.findall(r"\b(\d{4}-[^\s\\]+\.patch)\b", bzl_block.group(1))
        self.assertEqual(bzl_patches, indexed)


if __name__ == "__main__":
    unittest.main()
