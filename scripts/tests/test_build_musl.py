# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for scripts/build-musl.sh and its CI wiring."""

import os
import re
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

ROOT = SCRIPTS.parent
SCRIPT = SCRIPTS / "build-musl.sh"
CI = ROOT / ".github" / "workflows" / "ci.yml"


def write_exec(path: Path, content: str) -> None:
    path.write_text(content)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class BuildMusl(unittest.TestCase):
    def make_env(self, *, arch="x86_64", sysname="Linux", has_musl_gcc=True,
                 has_cross_gcc=False, readelf_interp=False, readelf_dynamic=False,
                 readelf_needed=False, readelf_debug_line=True):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        target_dir = tmp / "target"
        log_file = tmp / "cargo.log"

        write_exec(bin_dir / "uname", f"""#!/bin/bash
if [[ "$1" == "-m" ]]; then echo "{arch}"; else echo "{sysname}"; fi
""")
        if has_musl_gcc:
            write_exec(bin_dir / "musl-gcc", "#!/bin/bash\nexit 0\n")
        if has_cross_gcc:
            write_exec(bin_dir / "aarch64-linux-musl-gcc", "#!/bin/bash\nexit 0\n")

        write_exec(bin_dir / "cargo", f"""#!/bin/bash
set -euo pipefail
env | grep -E '^(CC_|CFLAGS_|CARGO_TARGET_)' | sort >> "{log_file}" || true
echo "ARGS: $*" >> "{log_file}"
target=""
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "--target" ]]; then target="$2"; shift 2; else shift; fi
done
dir="{target_dir}/$target/release"
mkdir -p "$dir"
for b in cladefsd cladefsctl cladefs-gc; do
    printf '#!/bin/sh\\n' > "$dir/$b"
    chmod +x "$dir/$b"
done
""")

        interp_line = "  INTERP  0x000000\n" if readelf_interp else ""
        dynamic_line = "  DYNAMIC 0x000000\n" if readelf_dynamic else ""
        needed_line = " 0x0000000000000001 (NEEDED) Shared library: [libc.so.6]\n" if readelf_needed else ""
        debug_line = "  [22] .debug_line PROGBITS\n" if readelf_debug_line else ""
        write_exec(bin_dir / "readelf", f"""#!/bin/bash
if [[ "$1" == "-l" ]]; then
    printf '  LOAD 0x000000\\n{interp_line}{dynamic_line}'
elif [[ "$1" == "-d" ]]; then
    printf ' 0x000000006ffffffb (FLAGS_1) Flags: PIE\\n{needed_line}'
elif [[ "$1" == "-S" ]]; then
    printf '  [1] .text PROGBITS\\n{debug_line}'
fi
""")

        # Symlink only the POSIX utilities build-musl.sh needs so host /usr/bin/musl-gcc cannot
        # leak into tests that check missing-compiler refusal on Linux.
        for util in ("bash", "chmod", "cp", "dirname", "env", "grep", "mkdir", "printf", "sort", "tr"):
            found = shutil.which(util)
            if found and not (bin_dir / util).exists():
                (bin_dir / util).symlink_to(found)

        env = {
            "PATH": str(bin_dir),
            "CARGO_TARGET_DIR": str(target_dir),
        }
        return tmp, target_dir, log_file, env

    def test_native_build_invokes_cargo_with_musl_gcc_and_copies_out(self):
        tmp, _, log_file, env = self.make_env(arch="x86_64")
        out_dir = tmp / "dist"
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT), "--out", str(out_dir)],
            env=env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        log = log_file.read_text()
        self.assertIn("CC_x86_64_unknown_linux_musl=musl-gcc", log)
        self.assertIn("CFLAGS_x86_64_unknown_linux_musl=-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu", log)
        self.assertIn("--release --locked", log)
        self.assertIn("--target x86_64-unknown-linux-musl", log)
        self.assertIn("-p cladefs --bin cladefsd --bin cladefsctl -p bucket_cell --bin cladefs-gc", log)
        for name in ("cladefsd", "cladefsctl", "cladefs-gc"):
            self.assertTrue((out_dir / name).is_file(), name)

    def test_cross_build_uses_arch_musl_gcc_and_sets_linker(self):
        _, _, log_file, env = self.make_env(arch="x86_64", has_musl_gcc=True, has_cross_gcc=True)
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT), "--target", "aarch64-unknown-linux-musl"],
            env=env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        log = log_file.read_text()
        self.assertIn("CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc", log)
        self.assertIn("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc", log)

    def test_refuses_missing_musl_compiler(self):
        _, _, _, env = self.make_env(arch="x86_64", has_musl_gcc=False)
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT), "--target", "x86_64-unknown-linux-musl"],
            env=env, capture_output=True, text=True)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("no musl C compiler found", done.stderr)

    def test_refuses_unsupported_target(self):
        _, _, _, env = self.make_env()
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT), "--target", "x86_64-unknown-linux-gnu"],
            env=env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 2)
        self.assertIn("unsupported target", done.stderr)

    def test_accepts_static_pie_without_interp_or_needed(self):
        _, _, _, env = self.make_env(readelf_dynamic=True, readelf_interp=False, readelf_needed=False)
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT)],
            env=env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_refuses_dynamically_linked_binary(self):
        _, _, _, env = self.make_env(readelf_interp=True)
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT)],
            env=env, capture_output=True, text=True)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("PT_INTERP", done.stderr)

    def test_refuses_dt_needed_shared_library(self):
        _, _, _, env = self.make_env(readelf_dynamic=True, readelf_needed=True)
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT)],
            env=env, capture_output=True, text=True)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("DT_NEEDED", done.stderr)

    def test_refuses_missing_debug_line(self):
        _, _, _, env = self.make_env(readelf_debug_line=False)
        done = subprocess.run(
            ["/bin/bash", str(SCRIPT)],
            env=env, capture_output=True, text=True)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn(".debug_line", done.stderr)

    def test_ci_builds_both_musl_targets(self):
        text = CI.read_text()
        match = re.search(r"^  musl-release:\n(.*?)(?=^  \S|\Z)", text, re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match, "ci.yml missing musl-release job")
        job = match.group(1)
        self.assertIn("x86_64-unknown-linux-musl", job)
        self.assertIn("aarch64-unknown-linux-musl", job)
        self.assertIn("scripts/build-musl.sh", job)


if __name__ == "__main__":
    unittest.main()
