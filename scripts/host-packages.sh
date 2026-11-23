#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Install the packages scripts/host-packages.txt lists, as root (through sudo, if not root), unless
# MARKER_DIR (default: the checkout's target) says this list went in already: its marker names the
# list, so a host that installed a different one installs again. A hotbed base's setup step runs it
# after every pull: a base built from nothing has none of them, as hotbed keeps no image of a
# project's own (board 9128).
#
#   scripts/host-packages.sh [MARKER_DIR]
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
markers=${1:-$repo/target}
source "$repo/scripts/lib/host-packages.sh"
if host_packages_installed "$markers"; then
    exit 0
fi
if [[ $(id -u) != 0 ]]; then
    exec sudo "$0" "$markers"
fi
host_packages_install "$markers"
