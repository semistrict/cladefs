#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Fails if the daemon's control, mount, or peer APIs (crates/cladefs/proto), or machined's
# (crates/machined/proto, board 13467), break a caller built from BASE's (doc-28, boards 10056,
# 10054): a VM manager is upgraded apart from the daemons it calls, and peer daemons upgrade
# rolling, so v1 only grows. `buf breaking` holds the working tree's .proto to BASE's by its FILE
# rules (each directory's buf.yaml), in the repository of the current directory; a directory BASE
# has none of is new, and holds to nothing yet. The only way past a break is the user's agreement,
# named in a `Breaks-format:` line by a commit of BASE..HEAD that changes the .proto, as
# scripts/format-freeze.py takes it of the commit breaking a corpus.
#
#   go/compat.sh BASE
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 BASE" >&2; exit 2; }
base=$1
buf=$(cd "$(dirname "$0")" && pwd)/buf.sh
root=$(git rev-parse --show-toplevel)

scratch=$(mktemp -d)
trap 'rm -r "$scratch"' EXIT
status=0
for protos in crates/cladefs/proto crates/machined/proto; do
    if ! git -C "$root" cat-file -e "$base:$protos" 2> /dev/null; then
        echo "$protos: new since $base, held to nothing yet"
        continue
    fi
    git -C "$root" archive "$base" "$protos" | tar -x -C "$scratch"
    if (cd "$root" && "$buf" breaking "$protos" --against "$scratch/$protos"); then
        continue
    fi
    agreed=$(git -C "$root" log --format=%B "$base..HEAD" -- "$protos" |
        grep -E '^Breaks-format:[[:space:]]*hotbed question [0-9]+' || true)
    if [[ -n $agreed ]]; then
        echo "$protos: breaks a caller of $base's, as the user agreed: $agreed"
        continue
    fi
    echo "$protos: breaks a caller of $base's, and no commit since changing it names the user's" \
        "agreement (a 'Breaks-format: hotbed question N' line, doc-28): a change that breaks a" \
        "caller is v2, served beside v1" >&2
    status=1
done
exit "$status"
