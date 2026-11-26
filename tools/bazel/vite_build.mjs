// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Sandboxed Vite 8 (Rolldown) build driver for //guide:dist.
// Rolldown is a native Rust binary that canonicalizes entry paths (bypassing aspect_rules_js's
// node:fs symlink virtualization); dereferencing the guide's input files into a local staging
// directory inside the Bazel sandbox gives Rolldown a single canonical root with zero symlinks.

import { cpSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { execFileSync } from "node:child_process";

const [outDirArg, ...viteArgs] = process.argv.slice(2);
if (!outDirArg) {
  console.error("usage: vite_build.mjs <out-dir> [vite-args...]");
  process.exit(2);
}

const guideDir = process.cwd();
const outDir = resolve(guideDir, outDirArg);
const stage = mkdtempSync(join(tmpdir(), "cladefs-guide-vite-"));

try {
  for (const entry of [
    "index.html",
    "package.json",
    "tsconfig.json",
    "vite.config.ts",
    "src",
    "tools",
  ]) {
    cpSync(join(guideDir, entry), join(stage, entry), {
      recursive: true,
      dereference: true,
    });
  }
  symlinkSync(join(guideDir, "node_modules"), join(stage, "node_modules"));
  const viteCli = join(guideDir, "node_modules", "vite", "bin", "vite.js");
  execFileSync(
    process.execPath,
    [viteCli, "build", "--outDir", outDir, ...viteArgs],
    { cwd: stage, stdio: "inherit" },
  );
} finally {
  rmSync(stage, { recursive: true, force: true });
}
