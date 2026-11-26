# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Native Bazel rule for running TLA+ model checking (TLC) against specs/."""

load("@rules_shell//shell:sh_test.bzl", "sh_test")

def tla_spec_test(
        name,
        spec_dir,
        srcs,
        models = None,
        tags = None,
        timeout = "long"):
    """Runs scripts/check-tla.sh hermetically using Bazel's fetched tla2tools.jar and remote JDK."""
    if models:
        spec_args = models
    else:
        # Derive SPEC/NAME model arguments from every .cfg in srcs (excluding deep models).
        spec_args = []
        for s in srcs:
            if s.endswith(".cfg"):
                base = s.rsplit("/", 1)[-1][:-4]
                spec_args.append("{}/{}".format(spec_dir, base))
    sh_test(
        name = name,
        srcs = ["//tools/bazel:run_tla.sh"],
        args = [
            "$(rootpath @tla2tools//file)",
            "$(JAVABASE)/bin/java",
        ] + spec_args,
        data = srcs + [
            "//scripts:check-tla.sh",
            "@rules_java//toolchains:current_java_runtime",
            "@tla2tools//file",
        ],
        tags = (tags or []) + ["tla"],
        timeout = timeout,
        toolchains = ["@rules_java//toolchains:current_java_runtime"],
    )
