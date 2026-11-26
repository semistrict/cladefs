#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Hermetic Bazel runner for scripts/check-tla.sh: stages Bazel's fetched tla2tools.jar
# into a temporary HOME cache and resolves the hermetic JDK from runfiles so check-tla.sh
# never accesses the network or host JDK.
set -euo pipefail

jar_src="$1"
java_src="$2"
shift 2

resolve_runfile() {
    local p="$1"
    if [[ -e "$p" ]]; then
        printf '%s/%s\n' "$(cd "$(dirname "$p")" && pwd)" "$(basename "$p")"
    elif [[ "$p" == external/* && -e "../${p#external/}" ]]; then
        local rel="../${p#external/}"
        printf '%s/%s\n' "$(cd "$(dirname "$rel")" && pwd)" "$(basename "$rel")"
    else
        echo "run_tla.sh: cannot locate runfile: $p (pwd=$PWD)" >&2
        exit 1
    fi
}

export JAVA="$(resolve_runfile "$java_src")"
jar_abs="$(resolve_runfile "$jar_src")"

tla_version="$(sed -n 's/^tla_version=//p' scripts/check-tla.sh)"
fake_home="${TEST_TMPDIR}/tla-home"
mkdir -p "${fake_home}/.cache/cladefs/tla"
ln -sf "${jar_abs}" "${fake_home}/.cache/cladefs/tla/tla2tools-${tla_version}.jar"
export HOME="${fake_home}"

exec bash scripts/check-tla.sh "$@"
