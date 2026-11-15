#!/usr/bin/env bash
# Copyright 2026 The fxdax Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run the tests of OpenBao's Transit engine as fxdax's KMS (TASK-411.16) with OpenBao's release
# binary, as scripts/lib/openbao.sh fetches it: each test starts a `bao server -dev` of its own.
# FXDAX_REQUIRE_OPENBAO has them fail rather than skip if they find no bao. The workspace's run
# has them too, skipping without bao; CI can run this script to be sure they ran. Arguments are
# passed to cargo nextest.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
source "$root/scripts/lib/openbao.sh"
FXDAX_BAO="$(openbao_binary)"
export FXDAX_BAO FXDAX_REQUIRE_OPENBAO=1
"$FXDAX_BAO" version
cargo nextest run --manifest-path "$root/Cargo.toml" -p extent_keys -p extent_store -E 'test(/openbao/)' "$@"
