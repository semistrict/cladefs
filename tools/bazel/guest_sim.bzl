# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Native Bazel C build rules for guest/sim (Linux fs/cladefs DAX userspace simulator).

Compiles fs/cladefs C sources and real Linux kernel sources (fs/dax.c, fs/iomap/iter.c,
lib/xarray.c, lib/genalloc.c) individually in Bazel actions with -nostdinc -D__KERNEL__
against generated alias headers, staged real kernel headers, and auto-generated lld stubs,
then links each guest/sim/tests/*.c binary and exposes Bazel test targets.
"""

load("@rules_shell//shell:sh_test.bzl", "sh_test")

SimHeadersInfo = provider(
    doc = "Staged real kernel headers and generated alias headers for guest/sim.",
    fields = {
        "real_headers": "depset of staged real/<header> files",
        "alias_headers": "depset of generated alias/<header> files",
        "real_dir": "exec-path to the staged real/ include root",
        "alias_dir": "exec-path to the generated alias/ include root",
        "kconfig": "File for staged real/linux/kconfig.h",
    },
)

SimObjectInfo = provider(
    doc = "Compiled object files for guest/sim.",
    fields = {
        "objects": "depset of .o Files",
    },
)

_REAL_HEADER_PATHS = [
    "linux/kconfig.h",
    "linux/list.h",
    "linux/poison.h",
    "linux/hash.h",
    "linux/hashtable.h",
    "linux/rculist.h",
    "linux/xarray.h",
    "linux/iomap.h",
    "linux/dax.h",
    "linux/genalloc.h",
    "linux/wait.h",
    "linux/wait_bit.h",
    "trace/events/fs_dax.h",
]

def _sim_headers_impl(ctx):
    real_files = []
    kconfig = None
    real_dir = None

    # Map basename/suffix from @amazon_linux_sim_kernel//:sim_real_headers to staged real/<path>.
    src_by_rel = {}
    for f in ctx.files.kernel_headers:
        for rel in _REAL_HEADER_PATHS:
            if f.path.endswith("/include/" + rel) or f.path.endswith("include/" + rel):
                src_by_rel[rel] = f
                break

    for rel in _REAL_HEADER_PATHS:
        if rel not in src_by_rel:
            fail("Missing real kernel header: include/" + rel)
        out = ctx.actions.declare_file("real/" + rel)
        ctx.actions.symlink(output = out, target_file = src_by_rel[rel])
        real_files.append(out)
        if rel == "linux/kconfig.h":
            kconfig = out
            # Strip "/linux/kconfig.h" to get the exec-path to real/.
            real_dir = out.path[:-len("/linux/kconfig.h")]

    # Generate 1-line alias headers from shim/ALIASES (parsed via a single fast shell action
    # declaring each alias file explicitly from the static mapping).
    alias_files = []
    alias_dir = None
    for target_hdr, aliases in ctx.attr.aliases.items():
        for alias in aliases:
            out = ctx.actions.declare_file("alias/" + alias)
            ctx.actions.write(
                output = out,
                content = "#include <{}>\n".format(target_hdr),
            )
            alias_files.append(out)
            if alias_dir == None:
                alias_dir = out.path[:-len("/" + alias)]

    return [
        DefaultInfo(files = depset(real_files + alias_files)),
        SimHeadersInfo(
            real_headers = depset(real_files),
            alias_headers = depset(alias_files),
            real_dir = real_dir,
            alias_dir = alias_dir,
            kconfig = kconfig,
        ),
    ]

sim_headers = rule(
    implementation = _sim_headers_impl,
    attrs = {
        "kernel_headers": attr.label(mandatory = True, allow_files = True),
        "aliases": attr.string_list_dict(mandatory = True),
    },
    doc = "Stages REAL_HEADERS and generates ALIASES forwarding headers for guest/sim.",
)

def _compile_kernel_c(ctx, src, out, hdr_info, extra_headers, sanitizer_flags):
    """Compiles one C source file in guest/sim's kernel-mode (-nostdinc -D__KERNEL__) environment."""
    cmd = """set -euo pipefail
res_dir="$(clang -print-resource-dir)"
exec clang -std=gnu11 -g -O1 -nostdinc -D__KERNEL__ -DKBUILD_MODNAME='"cladefs"' \\
    -isystem "${{res_dir}}/include" \\
    -I"{alias_dir}" -Iguest/sim/shim -I"{real_dir}" -Iguest/linux/include \\
    -iquote guest/linux/fs/cladefs -iquote guest/sim/tests \\
    -include "{kconfig}" \\
    -Wall -Wno-unused-function -Wno-unused-variable -Wno-pointer-sign -Wno-format \\
    -Wno-frame-address -Wno-gnu-variable-sized-type-not-at-end \\
    -fno-strict-aliasing -fno-omit-frame-pointer {san} \\
    -c "{src}" -o "{out}"
""".format(
        alias_dir = hdr_info.alias_dir,
        real_dir = hdr_info.real_dir,
        kconfig = hdr_info.kconfig.path,
        san = " ".join(sanitizer_flags),
        src = src.path,
        out = out.path,
    )
    ctx.actions.run_shell(
        inputs = depset(
            [src],
            transitive = [hdr_info.real_headers, hdr_info.alias_headers, extra_headers],
        ),
        outputs = [out],
        command = cmd,
        mnemonic = "SimKernelCompile",
        progress_message = "Compiling guest/sim C %{input}",
    )

def _sim_kernel_objects_impl(ctx):
    hdr_info = ctx.attr.headers[SimHeadersInfo]
    extra_headers = depset(ctx.files.extra_headers)
    san = list(ctx.attr.copts)
    objs = []
    for src in ctx.files.srcs:
        # Derive a unique output path from the source path.
        rel = src.short_path.replace("../", "ext/").replace(".c", ".o")
        out = ctx.actions.declare_file("objs/" + ctx.label.name + "/" + rel)
        _compile_kernel_c(ctx, src, out, hdr_info, extra_headers, san)
        objs.append(out)
    return [
        DefaultInfo(files = depset(objs)),
        SimObjectInfo(objects = depset(objs)),
    ]

sim_kernel_objects = rule(
    implementation = _sim_kernel_objects_impl,
    attrs = {
        "srcs": attr.label_list(mandatory = True, allow_files = [".c"]),
        "headers": attr.label(mandatory = True, providers = [SimHeadersInfo]),
        "extra_headers": attr.label_list(allow_files = [".h"]),
        "copts": attr.string_list(),
    },
    doc = "Compiles C sources in guest/sim's -nostdinc -D__KERNEL__ mode, one Bazel action per file.",
)

def _sim_host_object_impl(ctx):
    src = ctx.file.src
    out = ctx.actions.declare_file("objs/" + ctx.label.name + "/host.o")
    san = " ".join(ctx.attr.copts)
    ctx.actions.run_shell(
        inputs = [src] + ctx.files.hdrs,
        outputs = [out],
        command = 'clang -std=gnu11 -g -O1 -Wall -fno-omit-frame-pointer {san} -Iguest/sim/shim -c "{src}" -o "{out}"'.format(
            san = san,
            src = src.path,
            out = out.path,
        ),
        mnemonic = "SimHostCompile",
        progress_message = "Compiling guest/sim host C %{input}",
    )
    return [
        DefaultInfo(files = depset([out])),
        SimObjectInfo(objects = depset([out])),
    ]

sim_host_object = rule(
    implementation = _sim_host_object_impl,
    attrs = {
        "src": attr.label(mandatory = True, allow_single_file = [".c"]),
        "hdrs": attr.label_list(allow_files = [".h"]),
        "copts": attr.string_list(),
    },
    doc = "Compiles kernel/host.c against host libc (the one file outside -nostdinc).",
)

def _sim_stubs_object_impl(ctx):
    hdr_info = ctx.attr.headers[SimHeadersInfo]
    extra_headers = depset(ctx.files.extra_headers)
    all_objs = depset(transitive = [d[SimObjectInfo].objects for d in ctx.attr.deps])
    obj_list = all_objs.to_list()

    stubs_c = ctx.actions.declare_file("stubs/stubs.c")
    stubs_o = ctx.actions.declare_file("stubs/stubs.o")
    ldflags = " ".join(ctx.attr.linkopts)

    probe_cmd = """set -euo pipefail
tmp_out="$(mktemp)"
trap 'rm -f "$tmp_out"' EXIT
(clang {ldflags} -fuse-ld=lld -Wl,--no-demangle -Wl,--error-limit=0 "$@" -o "$tmp_out" 2>&1 || true) \\
    | sed -n 's/.*undefined symbol: //p' | sort -u \\
    | sed -e 's/.*/void &(void) {{ sim_panic("not simulated: &"); }}/' \\
          -e '1i #include <sim/base.h>' > "{stubs_c}"
# Guarantee a non-empty valid translation unit even if 0 symbols are missing.
if [ ! -s "{stubs_c}" ]; then
    echo '#include <sim/base.h>' > "{stubs_c}"
fi
""".format(ldflags = ldflags, stubs_c = stubs_c.path)

    ctx.actions.run_shell(
        inputs = all_objs,
        outputs = [stubs_c],
        arguments = [o.path for o in obj_list],
        command = probe_cmd,
        mnemonic = "SimGenStubs",
        progress_message = "Generating guest/sim undefined symbol stubs",
    )

    _compile_kernel_c(ctx, stubs_c, stubs_o, hdr_info, extra_headers, ctx.attr.copts)
    return [
        DefaultInfo(files = depset([stubs_o, stubs_c])),
        SimObjectInfo(objects = depset([stubs_o])),
    ]

sim_stubs_object = rule(
    implementation = _sim_stubs_object_impl,
    attrs = {
        "deps": attr.label_list(mandatory = True, providers = [SimObjectInfo]),
        "headers": attr.label(mandatory = True, providers = [SimHeadersInfo]),
        "extra_headers": attr.label_list(allow_files = [".h"]),
        "copts": attr.string_list(),
        "linkopts": attr.string_list(),
    },
    doc = "Generates and compiles stubs.c for unsimulated kernel symbols referenced by guest/sim.",
)

def _sim_binary_impl(ctx):
    hdr_info = ctx.attr.headers[SimHeadersInfo]
    extra_headers = depset(ctx.files.extra_headers)
    test_o = ctx.actions.declare_file("bin/" + ctx.label.name + ".o")
    _compile_kernel_c(ctx, ctx.file.src, test_o, hdr_info, extra_headers, ctx.attr.copts)

    all_objs = depset([test_o], transitive = [d[SimObjectInfo].objects for d in ctx.attr.deps])
    out = ctx.actions.declare_file(ctx.label.name)
    ldflags = " ".join(ctx.attr.linkopts)

    ctx.actions.run_shell(
        inputs = all_objs,
        outputs = [out],
        arguments = [o.path for o in all_objs.to_list()],
        command = 'clang {ldflags} -fuse-ld=lld -Wl,--no-demangle "$@" -o "{out}"'.format(
            ldflags = ldflags,
            out = out.path,
        ),
        mnemonic = "SimLink",
        progress_message = "Linking guest/sim binary %{output}",
    )
    return [
        DefaultInfo(
            executable = out,
            files = depset([out]),
        ),
    ]

sim_binary = rule(
    implementation = _sim_binary_impl,
    executable = True,
    attrs = {
        "src": attr.label(mandatory = True, allow_single_file = [".c"]),
        "deps": attr.label_list(mandatory = True, providers = [SimObjectInfo]),
        "headers": attr.label(mandatory = True, providers = [SimHeadersInfo]),
        "extra_headers": attr.label_list(allow_files = [".h"]),
        "copts": attr.string_list(),
        "linkopts": attr.string_list(),
    },
    doc = "Compiles and links one guest/sim test executable.",
)

def sim_test(name, binary, args = None, tags = None, timeout = "moderate"):
    """Runs a guest/sim test binary with leak/sanitizer checks enabled."""
    sh_test(
        name = name,
        srcs = ["//tools/bazel:run_sim_test.sh"],
        args = ["$(rootpath " + binary + ")"] + (args or []),
        data = [binary],
        env = {
            "ASAN_OPTIONS": "detect_leaks=1:detect_stack_use_after_return=0",
        },
        tags = (tags or []) + ["guest-sim"],
        target_compatible_with = ["@platforms//os:linux"],
        timeout = timeout,
    )
