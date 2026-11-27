# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Native Bazel rules for building the guest Linux kernel and its custom Rust sysroot + staticlib.

Uses Bazel's hermetic `@amazon_linux_kernel//:kernel.tar` archive (pinned commit verified in
`//tools/bazel:kernel.bzl`), builds `libcladefs_linux.a` as an independent cacheable target,
stamps `CLADEFS_BUILD_COMMIT` from Bazel's `stable-status.txt`, and checks every required
kernel config option and `cladefs_rust.o` string-function exclusion.
"""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

_KERNEL_SERIES = "6.18"
_KERNEL_BUILD_DATE = "2026-04-08T00:00:00Z"

def _guest_rust_staticlib_impl(ctx):
    arch = ctx.attr.arch
    if arch == "x86_64":
        target = "x86_64-unknown-none"
    elif arch == "arm64":
        target = "aarch64-unknown-none-softfloat"
    else:
        fail("Unsupported guest kernel arch: " + arch)

    rust_checks = ctx.attr.rust_checks[BuildSettingInfo].value
    out_archive = ctx.actions.declare_file("libcladefs_linux_{}.a".format(arch))

    cmd = """set -euo pipefail
export PATH="${{CARGO_HOME:-${{HOME:-/root}}/.cargo}}/bin:$PATH"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
root="$PWD"
rustflags="$("$root/{sysroot_script}" "{target}" "$work/rust")"
(
    cd "$root/guest/rust"
    CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS={rust_checks} \\
    CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS={rust_checks} \\
    RUSTFLAGS="$rustflags" \\
    CARGO_TARGET_DIR="$work/target" \\
    cargo build --locked --release -p cladefs-linux --target "{target}"
)
cp "$work/target/{target}/release/libcladefs_linux.a" "{out}"
# Strip compiler_builtins string functions so the Linux kernel's optimized versions are linked.
string_functions='memcpy|memmove|memset|memcmp|bcmp|strlen'
nm -A --defined-only "{out}" 2>/dev/null \\
    | awk -v names="^($string_functions)$" '$NF ~ names {{ split($1, f, ":"); print f[2] }}' \\
    | sort -u | while read -r member; do ar d "{out}" "$member"; done
""".format(
        sysroot_script = ctx.file.sysroot_script.path,
        target = target,
        rust_checks = "true" if rust_checks else "false",
        out = out_archive.path,
    )

    ctx.actions.run_shell(
        inputs = ctx.files.srcs + [ctx.file.sysroot_script],
        outputs = [out_archive],
        command = cmd,
        use_default_shell_env = True,
        mnemonic = "GuestRustStaticlib",
        progress_message = "Building guest kernel Rust staticlib ({})".format(target),
    )

    return [DefaultInfo(files = depset([out_archive]))]

guest_rust_staticlib = rule(
    implementation = _guest_rust_staticlib_impl,
    attrs = {
        "arch": attr.string(mandatory = True, values = ["x86_64", "arm64"]),
        "srcs": attr.label_list(mandatory = True, allow_files = True),
        "sysroot_script": attr.label(
            default = "//scripts:build-guest-sysroot.sh",
            allow_single_file = True,
        ),
        "rust_checks": attr.label(
            default = "//guest/linux:rust_checks",
            providers = [BuildSettingInfo],
        ),
    },
    doc = "Builds libcladefs_linux.a (no_std custom kernel sysroot + Fxfs staticlib) with string functions stripped.",
)

def _guest_linux_kernel_impl(ctx):
    arch = ctx.attr.arch
    if arch == "x86_64":
        guestarch = "x86_64"
        image_path = "vmlinux"
        image_name = "vmlinux"
        make_target = "vmlinux"
    elif arch == "arm64":
        guestarch = "aarch64"
        image_path = "arch/arm64/boot/Image"
        image_name = "Image"
        make_target = "Image"
    else:
        fail("Unsupported guest kernel arch: " + arch)

    out_image = ctx.actions.declare_file("out/{}/{}".format(arch, image_name))
    out_config = ctx.actions.declare_file("out/{}/cladefs.config".format(arch))

    profile = "1" if ctx.attr.profile[BuildSettingInfo].value else "0"
    debug = "1" if ctx.attr.debug[BuildSettingInfo].value else "0"
    kasan = "1" if ctx.attr.kasan[BuildSettingInfo].value else "0"
    lock_stat = "1" if ctx.attr.lock_stat[BuildSettingInfo].value else "0"
    coverage = "1" if ctx.attr.coverage[BuildSettingInfo].value else "0"

    cmd = """set -euo pipefail
root="$PWD"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
src="$work/linux"
mkdir -p "$src"
tar -xf "$root/{kernel_tar}" -C "$src"

# Apply cladefs's patch series and overlay guest/linux driver & filesystem sources.
for patch in \\
    0003-virtio-extent.patch \\
    0004-cladefs.patch \\
    0010-dax-read-replaces-no-entry-naming-its-page.patch \\
    0011-dax-a-folio-put-for-good-keeps-no-share-count.patch \\
    0014-fs-rotate-inodes-whose-page-cache-could-not-be-dropped.patch \\
    0015-fs-dont-lose-inodes-another-lru-walker-is-isolating.patch; do
    patch -d "$src" -p1 -s < "$root/guest/linux/patches/$patch"
done

mkdir -p "$src/drivers/virtio_extent" "$src/include/linux" "$src/fs/cladefs"
cp -R "$root/guest/linux/drivers/virtio_extent/." "$src/drivers/virtio_extent/"
cp "$root/guest/linux/include/linux/virtio_extent.h" "$src/include/linux/virtio_extent.h"
cp -R "$root/guest/linux/fs/cladefs/." "$src/fs/cladefs/"
cp "$root/{rust_staticlib}" "$src/fs/cladefs/libcladefs_linux.a"

# Assemble kernel configuration (Firecracker CI base + cladefs.config + variant flags).
configs="$root/guest/linux/configs"
awk 1 "$configs/microvm-kernel-ci-{guestarch}-{series}.config" \\
    "$configs/ci.config" \\
    "$configs/cladefs.config" > "$src/.config"
cp "$configs/cladefs.config" "$root/{out_config}"

if [[ "{arch}" == "x86_64" && "{profile}" == "1" ]]; then
    "$src/scripts/config" --file "$src/.config" --disable UNWINDER_GUESS --enable UNWINDER_FRAME_POINTER
fi
if [[ "{debug}" == "1" ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_KERNEL --enable PROVE_LOCKING \\
        --enable DEBUG_ATOMIC_SLEEP --enable DEBUG_LIST --enable KCSAN --enable DEBUG_FS \\
        --enable FAULT_INJECTION --enable FAILSLAB --enable FAIL_PAGE_ALLOC \\
        --enable FAULT_INJECTION_DEBUG_FS --enable FAULT_INJECTION_STACKTRACE_FILTER
fi
if [[ "{kasan}" == "1" ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_KERNEL --enable KASAN \\
        --enable KASAN_GENERIC --enable KFENCE --enable PROVE_LOCKING --enable DEBUG_ATOMIC_SLEEP \\
        --enable DEBUG_LIST --disable KCSAN --disable PVH
fi
if [[ "{lock_stat}" == "1" ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_KERNEL --enable LOCK_STAT
fi
if [[ "{coverage}" == "1" ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_FS --enable GCOV_KERNEL
fi

make_args=(ARCH="{arch}")
if [[ "{guestarch}" != "$(uname -m)" ]]; then
    make_args+=(CROSS_COMPILE="{guestarch}-linux-gnu-")
fi
make -C "$src" "${{make_args[@]}}" olddefconfig

for opt in CLADEFS_FS VIRTIO_EXTENT VIRTIO_PMEM SERIAL_8250_CONSOLE FTRACE BPF_EVENTS PANIC_ON_OOPS VIRTUALIZATION KVM; do
    grep -q "^CONFIG_${{opt}}=y" "$src/.config" || {{ echo "missing CONFIG_${{opt}}=y" >&2; exit 1; }}
done
grep -q '^CONFIG_PANIC_TIMEOUT=-1' "$src/.config"
if [[ "{arch}" == "x86_64" ]]; then
    grep -q '^CONFIG_KVM_AMD=y' "$src/.config"
    grep -q '^CONFIG_KVM_INTEL=y' "$src/.config"
fi

commit="$(awk '$1 == "STABLE_CLADEFS_BUILD_COMMIT" {{ print $2; exit }}' "$root/{status_file}")"
commit="${{commit:-unknown}}"

make -C "$src" "${{make_args[@]}}" -j"${{CLADEFS_GUEST_KERNEL_JOBS:-4}}" \\
    KBUILD_BUILD_USER=root KBUILD_BUILD_HOST=cladefs \\
    KBUILD_BUILD_TIMESTAMP="{build_date}" CLADEFS_BUILD_COMMIT="$commit" "{make_target}"

# Verify Rust's object defines no kernel string functions (memcpy/memset/etc.).
string_functions='memcpy|memmove|memset|memcmp|bcmp|strlen'
rust_symbols="$(nm "$src/fs/cladefs/cladefs_rust.o")"
if grep -E " [TtWw] ($string_functions)$" <<< "$rust_symbols" >&2; then
    echo "cladefs_rust.o defines the string functions above, which must be the kernel's" >&2
    exit 1
fi

cp "$src/{image_path}" "$root/{out_image}"
""".format(
        kernel_tar = ctx.file.kernel_archive.path,
        rust_staticlib = ctx.file.rust_staticlib.path,
        guestarch = guestarch,
        series = _KERNEL_SERIES,
        arch = arch,
        profile = profile,
        debug = debug,
        kasan = kasan,
        lock_stat = lock_stat,
        coverage = coverage,
        status_file = ctx.info_file.path,
        build_date = _KERNEL_BUILD_DATE,
        make_target = make_target,
        image_path = image_path,
        out_image = out_image.path,
        out_config = out_config.path,
    )

    ctx.actions.run_shell(
        inputs = ctx.files.srcs + [
            ctx.file.kernel_archive,
            ctx.file.rust_staticlib,
            ctx.info_file,
        ],
        outputs = [out_image, out_config],
        command = cmd,
        use_default_shell_env = True,
        mnemonic = "GuestLinuxKernel",
        progress_message = "Building guest Linux kernel ({})".format(arch),
    )

    return [
        DefaultInfo(files = depset([out_image, out_config])),
    ]

guest_linux_kernel = rule(
    implementation = _guest_linux_kernel_impl,
    attrs = {
        "arch": attr.string(mandatory = True, values = ["x86_64", "arm64"]),
        "srcs": attr.label_list(mandatory = True, allow_files = True),
        "rust_staticlib": attr.label(mandatory = True, allow_single_file = [".a"]),
        "kernel_archive": attr.label(
            default = "@amazon_linux_kernel//:kernel.tar",
            allow_single_file = [".tar"],
        ),
        "profile": attr.label(default = "//guest/linux:profile", providers = [BuildSettingInfo]),
        "debug": attr.label(default = "//guest/linux:debug", providers = [BuildSettingInfo]),
        "kasan": attr.label(default = "//guest/linux:kasan", providers = [BuildSettingInfo]),
        "lock_stat": attr.label(default = "//guest/linux:lock_stat", providers = [BuildSettingInfo]),
        "coverage": attr.label(default = "//guest/linux:coverage", providers = [BuildSettingInfo]),
        "rust_checks": attr.label(default = "//guest/linux:rust_checks", providers = [BuildSettingInfo]),
    },
    doc = "Builds the patched Amazon Linux 6.18 guest kernel (vmlinux / Image) hermetically from @amazon_linux_kernel + libcladefs_linux.a.",
)
