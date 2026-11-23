#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Audit the dependencies of every lock file with cargo-deny (MIT OR Apache-2.0), as deny.toml says
# (board 8605):
#
#   scripts/audit.sh policy       their licences, bans and sources: what only a commit changes, so
#                                 CI checks it on each push (ci.yml's deny job)
#   scripts/audit.sh advisories   RustSec's advisories against them: the nightly's (nightly.yml's
#                                 audit job), so that one published overnight fails no unrelated
#                                 push, and none lands unseen
#
# The locks: Cargo.lock, the workspace's; guest/rust/Cargo.lock, the guest kernel's Rust's;
# guest/tests/Cargo.lock, the guest tests' and their agent's; and vendor/firecracker.lock,
# Firecracker's as cladefs builds it (scripts/firecracker-cargo.sh). Each read --locked, by its
# workspace's own toolchain, every lock's findings said before it fails.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
case ${1:-} in
    policy) checks=(bans licenses sources) ;;
    advisories) checks=(advisories) ;;
    *) echo "Usage: $0 policy|advisories" >&2; exit 2 ;;
esac
deny=(deny --config "$root/deny.toml" --locked)
status=0
for workspace in . guest/rust guest/tests; do
    echo "scripts/audit.sh: $workspace/Cargo.lock" >&2
    (cd "$root/$workspace" && cargo "${deny[@]}" check "${checks[@]}") || status=1
done
echo "scripts/audit.sh: vendor/firecracker.lock" >&2
"$root/scripts/firecracker-cargo.sh" "${deny[@]}" check "${checks[@]}" || status=1
exit "$status"
