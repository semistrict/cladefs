#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail
bin="$1"
shift
exec "$bin" "$@"
