#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Bazel --workspace_status_command: emits STABLE_CLADEFS_BUILD_COMMIT for stamped builds
# (crates/cladefs, guest/linux), matching scripts/build-commit.sh.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
commit="$("$root/scripts/build-commit.sh" "$root")"
echo "STABLE_CLADEFS_BUILD_COMMIT ${commit}"
