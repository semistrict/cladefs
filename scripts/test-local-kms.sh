#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run the AWS KMS backend's tests (TASK-411.17) against local-kms, an emulator of AWS KMS
# (github.com/nsmithuk/local-kms, MIT), on this machine: install it at the version pinned below
# with `go install` (Go is needed, once), into target/local-kms, then run the tests named local_kms
# (ignored in the workspace's run), each of which starts an emulator of its own on a free port
# (extent_keys::testing::LocalAwsKms). No AWS account is needed, nor used: the SDK signs with the
# dummy credentials below, from the environment, as the standard chain finds them first, and the
# emulator checks no signature. CI's object-stores job runs it. Arguments are passed to cargo
# nextest.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# local-kms tags no release its module path takes (v3 without /v3): a pseudo-version, its commit
# 2a5e92d09ab6 of 2026-02-28.
version=v0.0.0-20260228092603-2a5e92d09ab6
bin=$root/target/local-kms/$version
if [[ ! -x $bin/local-kms ]]; then
    command -v go >/dev/null || { echo "$0: Go is needed to install local-kms" >&2; exit 1; }
    mkdir -p "$bin"
    GOBIN=$bin go install "github.com/nsmithuk/local-kms@$version"
fi

export CLADEFS_LOCAL_KMS=$bin/local-kms
export AWS_ACCESS_KEY_ID=cladefs-local-kms AWS_SECRET_ACCESS_KEY=cladefs-local-kms
# Nothing else of the chain is asked: no profile's, nor the instance's.
unset AWS_SESSION_TOKEN AWS_PROFILE
export AWS_EC2_METADATA_DISABLED=true
cargo nextest run --manifest-path "$root/Cargo.toml" -p extent_keys -p extent_store \
    --run-ignored only -E 'test(/local_kms/)' "$@"
