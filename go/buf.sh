#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# buf (Apache-2.0), at the version pinned here, run in Go: go/generate.sh's and go/compat.sh's.
#
#   go/buf.sh ARGS...
set -euo pipefail

exec go run github.com/bufbuild/buf/cmd/buf@v1.75.0 "$@"
