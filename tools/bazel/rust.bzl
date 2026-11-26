# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared Bazel macros for cladefs Rust targets."""

load("@rules_python//python:defs.bzl", "py_test")
load(
    "@rules_rust//rust:defs.bzl",
    "rust_binary",
    "rust_library",
    "rust_proc_macro",
    "rust_test",
)

# Default test environment matching .cargo/config.toml so daemons started by tests
# leave zero memory headroom (board 5662) and use the test mutual-TLS credentials (board 8437).
CLADEFS_TEST_ENV = {
    "CLADEFSD_HEADROOM": "0",
    "CLADEFSD_PEER_TLS": "crates/cladefs/tests/peer-tls",
}

# Maps @crates//:madsim-tokio to the `tokio` crate name for crates that use workspace tokio
# (see //tools/bazel:cargo_manifests.bzl).
MADSIM_TOKIO_ALIAS = {
    "@crates//:madsim-tokio": "tokio",
}

def with_madsim_tokio(base_aliases = None):
    merged = dict(MADSIM_TOKIO_ALIAS)
    if base_aliases:
        merged.update(base_aliases)
    return merged

def cladefs_rust_library(
        name,
        edition = "2024",
        rustc_env = None,
        **kwargs):
    merged_rustc_env = {"CARGO_MANIFEST_DIR": native.package_name()}
    if rustc_env:
        merged_rustc_env.update(rustc_env)
    rust_library(
        name = name,
        edition = edition,
        rustc_env = merged_rustc_env,
        **kwargs
    )

def cladefs_rust_proc_macro(
        name,
        edition = "2024",
        **kwargs):
    rust_proc_macro(
        name = name,
        edition = edition,
        **kwargs
    )

def cladefs_rust_binary(
        name,
        edition = "2024",
        rustc_env = None,
        **kwargs):
    merged_rustc_env = {"CARGO_MANIFEST_DIR": native.package_name()}
    if rustc_env:
        merged_rustc_env.update(rustc_env)
    rust_binary(
        name = name,
        edition = edition,
        rustc_env = merged_rustc_env,
        **kwargs
    )

def cladefs_rust_test(
        name,
        edition = "2024",
        env = None,
        data = None,
        rustc_env = None,
        isolated = False,
        args = None,
        shard_count = None,
        timeout = None,
        tags = None,
        target_compatible_with = None,
        **kwargs):
    merged_env = dict(CLADEFS_TEST_ENV)
    if env:
        merged_env.update(env)
    merged_data = list(data) if data else []
    if (
        "//crates/cladefs:peer_tls_testdata" not in merged_data and
        ":peer_tls_testdata" not in merged_data
    ):
        merged_data.append("//crates/cladefs:peer_tls_testdata")
    merged_rustc_env = {
        "CARGO_MANIFEST_DIR": native.package_name(),
        "CARGO_TARGET_TMPDIR": "/tmp",
    }
    if rustc_env:
        merged_rustc_env.update(rustc_env)

    if isolated:
        bin_name = name + "_bin"
        bin_tags = list(tags) if tags else []
        if "manual" not in bin_tags:
            bin_tags.append("manual")
        rust_test(
            name = bin_name,
            edition = edition,
            env = merged_env,
            data = merged_data,
            rustc_env = merged_rustc_env,
            tags = bin_tags,
            target_compatible_with = target_compatible_with,
            **kwargs
        )
        py_kwargs = {}
        if shard_count != None:
            py_kwargs["shard_count"] = shard_count
        if timeout != None:
            py_kwargs["timeout"] = timeout
        if tags != None:
            py_kwargs["tags"] = tags
        if target_compatible_with != None:
            py_kwargs["target_compatible_with"] = target_compatible_with
        py_test(
            name = name,
            srcs = ["//tools/bazel:run_isolated_rust_test.py"],
            main = "//tools/bazel:run_isolated_rust_test.py",
            args = ["$(rootpath :{})".format(bin_name)] + (list(args) if args else []),
            data = [":" + bin_name] + merged_data,
            env = merged_env,
            **py_kwargs
        )
        return

    extra = {}
    if args != None:
        extra["args"] = args
    if shard_count != None:
        extra["shard_count"] = shard_count
    if timeout != None:
        extra["timeout"] = timeout
    if tags != None:
        extra["tags"] = tags
    if target_compatible_with != None:
        extra["target_compatible_with"] = target_compatible_with
    rust_test(
        name = name,
        edition = edition,
        env = merged_env,
        data = merged_data,
        rustc_env = merged_rustc_env,
        **dict(kwargs, **extra)
    )
