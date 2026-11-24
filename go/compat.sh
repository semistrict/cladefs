#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Fails if the daemon's control, mount, or peer APIs (crates/cladefs/proto) break a caller built
# from BASE's (doc-28, boards 10056, 10054): a VM manager is upgraded apart from the daemons it
# calls, and peer daemons upgrade rolling, so v1 only grows. `buf breaking` holds the working
# tree's .proto to BASE's by its FILE rules (crates/cladefs/proto/buf.yaml), in the repository of
# the current directory. The only way past a break is the user's agreement, named in a
# `Breaks-format:` line by a commit of BASE..HEAD that changes the .proto, as
# scripts/format-freeze.py takes it of the commit breaking a corpus.
#
#   go/compat.sh BASE
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 BASE" >&2; exit 2; }
base=$1
buf=$(cd "$(dirname "$0")" && pwd)/buf.sh
root=$(git rev-parse --show-toplevel)
protos=crates/cladefs/proto

scratch=$(mktemp -d)
trap 'rm -r "$scratch"' EXIT
git -C "$root" archive "$base" "$protos" | tar -x -C "$scratch"
if (cd "$root" && "$buf" breaking "$protos" --against "$scratch/$protos"); then
    exit 0
fi
agreed=$(git -C "$root" log --format=%B "$base..HEAD" -- "$protos" |
    grep -E '^Breaks-format:[[:space:]]*hotbed question [0-9]+' || true)
if [[ -n $agreed ]]; then
    echo "$protos: breaks a caller of $base's, as the user agreed: $agreed"
    exit 0
fi
echo "$protos: breaks a caller of $base's, and no commit since changing it names the user's agreement (a" \
    "'Breaks-format: hotbed question N' line, doc-28): a change that breaks a caller is v2, served" \
    "beside v1" >&2
exit 1
