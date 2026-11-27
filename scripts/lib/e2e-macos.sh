# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# What scripts/test-e2e.sh needs on an arm64 Mac, where the guests run on libmachine's
# Firecracker-compatible VMM (Hypervisor.framework) and cladefsd natively (board 14336), and which
# sources this: a case-sensitive volume for its results, and the guest's tools a Linux host has.
# Bash 3.2, macOS's own, runs it. Needs scripts/lib/user-cache.sh and scripts/lib/containers.sh.

# E2E_MACOS_VOLUME is the volume's name, and so its mount point's, /Volumes/NAME.
E2E_MACOS_VOLUME=cladefs-runs

# e2e_macos_volume prints the mount point of a case-sensitive APFS volume for the run's results,
# made first if there is none: a volume of its own in the Mac's boot container, sharing its free
# space (diskutil; no root). A Mac's own volumes are case-insensitive, where a Linux root's names
# collide (Fedora's terminfo/E/Eterm and terminfo/e/eterm), and the runner refuses to extract one
# there (guest/tests rootfs.rs). Not a disk image, as it was (hdiutil's sparse image, attached at
# /Volumes/cladefs-e2e, which can go): each sync there took ~30 ms, not ~0.2 ms, which held the
# daemons' commits ~250 ms before they were durable, failing the pgbench cases' check of doc-12's
# median (50 ms; board 14462). It stays for the next run, as a cache directory does; `diskutil apfs
# deleteVolume cladefs-runs` removes it and frees its space.
e2e_macos_volume() {
    local mount=/Volumes/$E2E_MACOS_VOLUME boot
    boot=$(e2e_macos_container /) || return
    if [[ ! -d $mount ]]; then
        if diskutil info "$E2E_MACOS_VOLUME" > /dev/null 2>&1; then
            diskutil mount "$E2E_MACOS_VOLUME" >&2 || return
        else
            diskutil apfs addVolume "$boot" 'Case-sensitive APFS' "$E2E_MACOS_VOLUME" >&2 ||
                return
        fi
    fi
    if [[ $(e2e_macos_container "$mount") != "$boot" ]]; then
        echo "$mount isn't a volume of the boot container $boot (a disk image's syncs are" \
            "~100 times slower): hdiutil detach $mount" >&2
        return 1
    fi
    e2e_macos_case_sensitive "$mount" || return
    echo "$mount"
}

# e2e_macos_container PATH prints the APFS container of the volume PATH is on (disk3).
e2e_macos_container() {
    diskutil info -plist "$1" | plutil -extract APFSContainerReference raw - ||
        { echo "$1 is on no APFS volume" >&2; return 1; }
}

# e2e_macos_case_sensitive DIR fails, saying so, unless DIR is on a case-sensitive filesystem.
e2e_macos_case_sensitive() {
    local probe
    probe=$(mktemp -d "$1/.case.XXXXXX") || return
    touch "$probe/a"
    if [[ -e $probe/A ]]; then
        rm -r "$probe"
        echo "$1 is on a case-insensitive filesystem, where a Linux root's names collide" >&2
        return 1
    fi
    rm -r "$probe"
}

# e2e_macos_busybox prints the path of a static BusyBox for the arm64 guest, the containers' image
# of fxfs-guest-docker-dax (CLADEFS_BUSYBOX), which a Linux host has as /bin/busybox: Alpine's
# busybox-static, taken from its package once, by the container engine that runs here, into the
# user's cache.
e2e_macos_busybox() {
    local busybox engine
    busybox=$(user_cache)/busybox-static-aarch64-alpine-3.22 || return
    if [[ ! -x $busybox ]]; then
        engine=$(container_engine) || return
        "$engine" run --rm --platform linux/arm64 alpine:3.22 \
            sh -c 'apk add -q --no-cache busybox-static >&2 && cat /bin/busybox.static' \
            > "$busybox.partial" || { rm -f "$busybox.partial"; return 1; }
        chmod +x "$busybox.partial"
        mv "$busybox.partial" "$busybox"
    fi
    echo "$busybox"
}

# e2e_macos_e2fsprogs prints the directory of e2fsprogs' mkfs.ext4 (GPL-2.0), which macOS lacks:
# Homebrew's keg-only e2fsprogs, which `brew install e2fsprogs` installs, which the cases' root
# filesystems and ext4 images are made with.
e2e_macos_e2fsprogs() {
    local prefix
    prefix=$(brew --prefix e2fsprogs 2> /dev/null) && [[ -x $prefix/sbin/mkfs.ext4 ]] ||
        { echo "no mkfs.ext4: brew install e2fsprogs" >&2; return 1; }
    echo "$prefix/sbin"
}
