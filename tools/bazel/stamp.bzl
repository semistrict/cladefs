# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Native Bazel stamping helpers turning STABLE_CLADEFS_BUILD_COMMIT into rustc_env_files."""

def _cladefs_stamp_env_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".env")
    ctx.actions.run_shell(
        inputs = [ctx.info_file],
        outputs = [out],
        command = """set -euo pipefail
commit="$(awk '$1 == "STABLE_CLADEFS_BUILD_COMMIT" { print $2; exit }' "$1")"
printf 'CLADEFS_BUILD_COMMIT=%s\\n' "${commit:-unknown}" > "$2"
""",
        arguments = [ctx.info_file.path, out.path],
        mnemonic = "CladefsStampEnv",
        progress_message = "Extracting CLADEFS_BUILD_COMMIT stamp",
    )
    return [DefaultInfo(files = depset([out]))]

cladefs_stamp_env = rule(
    implementation = _cladefs_stamp_env_impl,
    doc = "Writes a rustc_env_files entry setting CLADEFS_BUILD_COMMIT from Bazel's stable-status.txt.",
)
