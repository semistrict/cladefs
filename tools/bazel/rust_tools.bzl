# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Exposes Bazel's hermetic cargo + rustc + rust-std toolchain files to tests that invoke cargo."""

def _rust_host_toolchain_files_impl(ctx):
    rust_tc = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    cargo_tc = ctx.toolchains["@rules_rust//cargo:toolchain_type"]
    files = depset(
        direct = [cargo_tc.cargo, rust_tc.rustc],
        transitive = [rust_tc.all_files],
    )
    return [
        DefaultInfo(
            files = files,
            runfiles = ctx.runfiles(transitive_files = files),
        ),
        platform_common.TemplateVariableInfo({
            "HERMETIC_CARGO": cargo_tc.cargo.short_path,
            "HERMETIC_RUSTC": rust_tc.rustc.short_path,
        }),
    ]

rust_host_toolchain_files = rule(
    implementation = _rust_host_toolchain_files_impl,
    toolchains = [
        "@rules_rust//rust:toolchain_type",
        "@rules_rust//cargo:toolchain_type",
    ],
    doc = "Collects hermetic cargo, rustc, and sysroot files from the active rules_rust toolchains.",
)
