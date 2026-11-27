# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Shared musl build and verification helpers for cladefs's static Linux binaries (cladefsd,
# cladefsctl, cladefs-gc). Sourced by scripts/build-musl.sh, scripts/test-e2e.sh,
# scripts/gc-sweep.sh, scripts/lib/fork-time-linux-host.sh, and scripts/test.sh.

# musl_setup [TARGET]
# Validates TARGET (default: $(uname -m)-unknown-linux-musl) and sets:
#   MUSL_TARGET  the rustc target triple (x86_64-unknown-linux-musl or aarch64-unknown-linux-musl)
#   MUSL_ENV     array of KEY=VALUE assignments for env(1) (CC_*, CFLAGS_*, and cross linker)
musl_setup() {
    local target="${1:-}"
    local host_arch
    host_arch="$(uname -m)"
    if [[ -z $target ]]; then
        target="${host_arch}-unknown-linux-musl"
    fi

    local target_arch
    case "$target" in
        x86_64-unknown-linux-musl) target_arch=x86_64 ;;
        aarch64-unknown-linux-musl) target_arch=aarch64 ;;
        *)
            echo "musl: unsupported target '$target' (expected x86_64-unknown-linux-musl or aarch64-unknown-linux-musl)" >&2
            return 2
            ;;
    esac

    local target_under="${target//-/_}"
    local target_upper
    target_upper="$(printf '%s' "$target_under" | tr '[:lower:]' '[:upper:]')"
    local cc_var="CC_${target_under}"
    local cflags_var="CFLAGS_${target_under}"
    local linker_var="CARGO_TARGET_${target_upper}_LINKER"

    # Never silently fall back to the host glibc gcc.
    local cc="${!cc_var:-}"
    if [[ -z $cc ]]; then
        if [[ $(uname -s) == Linux && $host_arch == "$target_arch" ]] && command -v musl-gcc >/dev/null 2>&1; then
            cc="musl-gcc"
        elif command -v "${target_arch}-linux-musl-gcc" >/dev/null 2>&1; then
            cc="${target_arch}-linux-musl-gcc"
        else
            echo "musl: no musl C compiler found for $target (install musl-tools for native builds, or set $cc_var to a musl cross-compiler such as ${target_arch}-linux-musl-gcc)" >&2
            return 1
        fi
    fi

    local cflags="${!cflags_var:-}"
    if [[ -z $cflags && $cc == "musl-gcc" && $(uname -s) == Linux && $host_arch == "$target_arch" ]]; then
        # musl-gcc's sysroot lacks Linux kernel headers, which C deps may include: put the host's
        # headers after musl's own (matching Firecracker's build in scripts/test-e2e.sh).
        cflags="-idirafter /usr/include -idirafter /usr/include/${host_arch}-linux-gnu"
    fi

    MUSL_TARGET="$target"
    MUSL_ENV=("${cc_var}=${cc}")
    if [[ -n $cflags ]]; then
        MUSL_ENV+=("${cflags_var}=${cflags}")
    fi
    if [[ -z ${!linker_var:-} && $host_arch != "$target_arch" ]]; then
        MUSL_ENV+=("${linker_var}=${cc}")
    fi
}

# musl_verify_static [--require-debug-line] BIN...
# Fails unless every BIN is a statically linked ELF (static or static-PIE: no PT_INTERP and no
# DT_NEEDED shared libraries), and with --require-debug-line also verifies .debug_line is present.
musl_verify_static() {
    local require_debug_line=0
    if [[ ${1:-} == --require-debug-line ]]; then
        require_debug_line=1
        shift
    fi
    local bin
    for bin in "$@"; do
        [[ -x $bin ]] || { echo "musl: missing binary $bin" >&2; return 1; }
        if command -v readelf >/dev/null 2>&1; then
            if readelf -l "$bin" | grep -qE '\bINTERP\b'; then
                echo "musl: $bin has a PT_INTERP dynamic linker (expected static musl binary)" >&2
                readelf -l "$bin" | grep -E '\bINTERP\b' >&2 || true
                return 1
            fi
            if readelf -d "$bin" 2>/dev/null | grep -q '(NEEDED)'; then
                echo "musl: $bin has DT_NEEDED shared libraries (expected static musl binary)" >&2
                readelf -d "$bin" | grep '(NEEDED)' >&2 || true
                return 1
            fi
            if (( require_debug_line )) && ! readelf -S "$bin" | grep -q '\.debug_line'; then
                echo "musl: $bin is missing .debug_line (needed by /z/pprof)" >&2
                return 1
            fi
        elif command -v ldd >/dev/null 2>&1; then
            local ldd_out
            ldd_out="$(ldd "$bin" 2>&1 || true)"
            if ! grep -qE 'not a dynamic executable|statically linked' <<< "$ldd_out"; then
                echo "musl: $bin is dynamically linked: $ldd_out" >&2
                return 1
            fi
        else
            echo "musl: neither readelf nor ldd found to verify static linking" >&2
            return 1
        fi
    done
}
