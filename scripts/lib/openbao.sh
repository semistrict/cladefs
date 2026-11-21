# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Sourced by the scripts that run cladefs's tests against OpenBao (openbao.org, MPL-2.0), the real
# KMS a test starts for itself, `bao server -dev` (extent_keys::testing::openbao::DevServer,
# TASK-411.16): its release's binary, fetched once and checked against its digest (its
# checksums.txt). CLADEFS_BAO names a bao binary to run instead of the release's; the tests run
# CLADEFS_BAO's, else PATH's.

# The release run, and each platform's archive's SHA-256.
OPENBAO_VERSION=2.7.2

# user_cache, where the release is fetched to.
source "$(dirname "${BASH_SOURCE[0]}")/user-cache.sh"

# openbao_binary prints the path of the release's binary for this platform, fetching it once into
# the cache (XDG_CACHE_HOME's, or ~/.cache's, cladefs/openbao-VERSION-PLATFORM: user-cache.sh's).
openbao_binary() {
    if [[ -n ${CLADEFS_BAO:-} ]]; then
        echo "$CLADEFS_BAO"
        return
    fi
    local platform sha256 dir archive
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64)
            platform=linux_amd64
            sha256=582b1f3c2fdc5afd443b2d57a225ef654de75adea609fdb66731fd582a9cbc96 ;;
        Linux-aarch64)
            platform=linux_arm64
            sha256=5b97bf816fa820e5e521abec1db19c6161fcca2f666cf4a84e335089c0e83edd ;;
        Darwin-arm64)
            platform=darwin_arm64
            sha256=2b4fc131910f4b075b2e1c9a9de67c461489491a8f03fbdc73387294d67d9547 ;;
        *) echo "OpenBao $OPENBAO_VERSION has no release for $(uname -s) $(uname -m)" >&2; return 1 ;;
    esac
    dir=$(user_cache "openbao-$OPENBAO_VERSION-$platform") || return
    if [[ ! -x $dir/bao ]]; then
        archive="$dir.tar.gz"
        curl -fsSL --retry 3 -o "$archive" "https://github.com/openbao/openbao/releases/download/v$OPENBAO_VERSION/openbao_${OPENBAO_VERSION}_$platform.tar.gz" ||
            return
        echo "$sha256  $archive" | shasum -a 256 --check --quiet || { rm -f "$archive"; return 1; }
        tar -xzf "$archive" -C "$dir" bao && chmod +x "$dir/bao" || return
        rm "$archive"
    fi
    echo "$dir/bao"
}
