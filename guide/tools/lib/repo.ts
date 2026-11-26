// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// What the guide's tools know of the repo: where it is, where its code comes from, and git.

import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { Origin } from "../../src/data/types.ts";

/** The repo's root: the guide lives in its `guide/`, unless CLADEFS_REPO names another. */
export const REPO = resolve(
  process.env.CLADEFS_REPO ?? join(dirname(fileURLToPath(import.meta.url)), "..", "..", ".."),
);

/** The guide's own directory. */
export const GUIDE = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/**
 * Where code comes from, as docs/code.md sorts it:
 * - `cladefs`: cladefs's own;
 * - `ported`: cladefs's own code citing a Fuchsia source it ports or excerpts;
 * - `fuchsia`: copied from Fuchsia, as upstream wrote it;
 * - `fuchsia-changed`: copied from Fuchsia, deviating from upstream as its leading comment says;
 * - `stand-in`: cladefs's stand-in for a crate only Fuchsia's build has.
 */
export type { Origin };

export const ORIGINS: readonly Origin[] = [
  "cladefs",
  "ported",
  "fuchsia-changed",
  "fuchsia",
  "stand-in",
];

/** A Fuchsia source cited, as scripts/upstream-changes.py's CITATION reads them. */
export const CITATION = /`((?:zircon|src|sdk|third_party|prebuilt)\/[^`\s]+)`/;

/** The stand-in crates of docs/code.md's table "`crates/`: stand-ins for Fuchsia-only pieces". */
export function standIns(repo = REPO): Set<string> {
  const doc = readFileSync(join(repo, "docs", "code.md"), "utf8");
  const section = doc.split("## `crates/`: stand-ins for Fuchsia-only pieces")[1];
  if (section === undefined) throw new Error("docs/code.md has no stand-ins section");
  const body = section.split("\n## ")[0] ?? section;
  const table = body.split("\n\n").find((part) => part.trimStart().startsWith("|"));
  if (table === undefined) return new Set();
  const names = new Set<string>();
  for (const row of table.split("\n").slice(2)) {
    const cell = row.split("|")[1] ?? "";
    // `fidl-fuchsia-io`, `-fxfs`: a suffix names a crate of the first's prefix.
    let prefix = "";
    for (const [, name] of cell.matchAll(/`([^`]+)`/g)) {
      if (name.startsWith("-") && prefix) names.add(prefix + name.slice(1));
      else {
        names.add(name);
        prefix = name.slice(0, name.lastIndexOf("-") + 1);
      }
    }
  }
  return names;
}

/** The comments before a file's first line of code. */
export function leadingComment(text: string): string {
  const lines: string[] = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trimStart();
    if (line && !(trimmed.startsWith("//") || trimmed.startsWith("/*") || trimmed.startsWith("*")))
      break;
    lines.push(line);
  }
  return lines.join("\n");
}

/** Each crate under crates/ by its origin, from its Cargo.toml's citation of Fuchsia. */
export function crateOrigins(repo = REPO): Map<string, Origin> {
  const stand = standIns(repo);
  const origins = new Map<string, Origin>();
  for (const name of readdirSync(join(repo, "crates")).sort()) {
    const manifest = join(repo, "crates", name, "Cargo.toml");
    if (!existsSync(manifest)) continue;
    if (stand.has(name)) origins.set(name, "stand-in");
    else origins.set(name, CITATION.test(readFileSync(manifest, "utf8")) ? "fuchsia" : "cladefs");
  }
  return origins;
}

/**
 * A file's origin: its crate's, refined by its leading comment. In a crate copied from Fuchsia,
 * a file under cladefs's copyright is cladefs's own, and one under Fuchsia's whose leading comment
 * names cladefs deviates from upstream; in cladefs's crates, a file citing a Fuchsia source ports it.
 */
export function fileOrigin(crate: Origin, text: string): Origin {
  const head = leadingComment(text);
  if (crate === "fuchsia") {
    if (head.includes("The cladefs Authors")) return "cladefs";
    return head.includes("cladefs") ? "fuchsia-changed" : "fuchsia";
  }
  if (crate === "cladefs" && CITATION.test(head)) return "ported";
  return crate;
}

/** The origin of the file at `path`, relative to the repo. */
export function originOf(path: string, crates: Map<string, Origin>, repo = REPO): Origin {
  const match = /^crates\/([^/]+)\//.exec(path);
  const crate = match ? (crates.get(match[1]) ?? "cladefs") : "cladefs";
  return fileOrigin(crate, readFileSync(join(repo, path), "utf8"));
}

export function git(args: string[], cwd = REPO): string {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}

/** The repo's commit, and the Firecracker submodule's, which links into its own repo name. */
export function revisions(repo = REPO): { commit: string; firecracker: string } {
  return {
    commit: process.env.CLADEFS_BUILD_COMMIT || git(["rev-parse", "HEAD"], repo),
    firecracker:
      process.env.CLADEFS_FIRECRACKER_COMMIT ||
      git(["rev-parse", "HEAD"], join(repo, "vendor", "firecracker")),
  };
}
