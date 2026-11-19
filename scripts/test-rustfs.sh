#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run the cells' tests against RustFS, an S3 server (github.com/rustfs/rustfs, Apache-2.0), on
# this machine, as scripts/lib/rustfs.sh starts it, a process, its bucket made: run the S3
# tests (ignored in the workspace's run), collect what they left, and stop it. CI's object-stores
# job runs it (TASK-149.20). Arguments are passed to cargo nextest.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
source "$root/scripts/lib/rustfs.sh"
source "$root/scripts/lib/test-kms.sh"
rustfs_start
trap rustfs_stop EXIT
CLADEFS_S3_URL=s3://cladefs/test cargo nextest run --manifest-path "$root/Cargo.toml" \
    -p bucket_cell -p cladefs --run-ignored only -E 'test(/rustfs/)' "$@"
# The collector, over what the tests left. cladefs-gc refuses to collect with no KMS (TASK-411.28):
# the bucket KMS of a test master key, unless the environment names one. Its alone: given it, the
# tests' processes would choose the bucket KMS too, over a host bucket they name none of, and fail
# (TASK-645); given none, they encrypt under the tests' KMS.
(
    test_master_key
    cargo run --quiet --manifest-path "$root/Cargo.toml" -p bucket_cell --bin cladefs-gc -- \
        s3://cladefs/test --grace 0
)
