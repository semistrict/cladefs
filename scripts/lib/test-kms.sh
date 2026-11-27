#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# cladefs: a master key for the scripts that measure or test cladefs's processes with no KMS of
# their own (TASK-411.28): every cell is encrypted, and every process that opens cells (cladefsd,
# cladefsctl, cladefs-gc-fill; cladefs-gc only to rotate master keys) refuses to
# start given neither CLADEFS_KMS nor a master key. test_master_key exports CLADEFS_MASTER_KEY, a
# key known to all, never a host's, unless the environment names a KMS or gives a master key
# already; the bucket KMS then keeps tenants' KEKs in the processes' bucket. An old pin's binaries,
# built before encryption, ignore it. Sourced; scripts/test-e2e.sh makes a KMS of its own instead
# (`runner kms`).

# The tests' master key, in hexadecimal: 32 bytes of 0x7e, as the Rust tests' (bucket_cell's
# tests/gc.rs).
TEST_MASTER_KEY=7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e

test_master_key() {
    if [[ -z ${CLADEFS_KMS:-}${CLADEFS_MASTER_KEY:-}${CLADEFS_MASTER_KEY_FILE:-} ]]; then
        export CLADEFS_MASTER_KEY=$TEST_MASTER_KEY
    fi
}
