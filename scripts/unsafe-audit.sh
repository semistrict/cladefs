#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# An independent label for each unsafe site's SAFETY claim (TASK-444.4): Codex, read-only, given
# scripts/unsafe-audit.md and the files, says of each site whether its claim is Correct, Missing or
# Incorrect, with the precondition and why, as std-unsafe-doc has a second model check the first's.
# Labels are proposals: an Incorrect one counts once a test, Miri run or schedule shows it.
#
#   unsafe-audit.sh --out FILE PATH...
#
# FILE gets Codex's last message, its labels; FILE.log what else it said. The model is Codex's
# strongest, gpt-6-astra at xhigh, unless CLADEFS_AUDIT_MODEL and CLADEFS_AUDIT_EFFORT say
# otherwise.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=""
if [[ ${1:-} == --out ]]; then
    out=$2
    shift 2
fi
[[ -n $out && $# -gt 0 ]] || { echo "usage: $0 --out FILE PATH..." >&2; exit 2; }
{
    cat "$root/scripts/unsafe-audit.md"
    printf -- '- %s\n' "$@"
} | codex exec -m "${CLADEFS_AUDIT_MODEL:-gpt-6-astra}" \
    -c model_reasoning_effort="${CLADEFS_AUDIT_EFFORT:-xhigh}" \
    --sandbox read-only --ephemeral -C "$root" --output-last-message "$out" - > "$out.log" 2>&1
cat "$out"
