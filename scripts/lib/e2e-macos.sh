# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# What scripts/test-e2e.sh needs on an arm64 Mac, where the guests run on libmachine's
# Firecracker-compatible VMM (Hypervisor.framework) and cladefsd natively (board 14336), and which
# sources this: a case-sensitive volume for its results, and the guest's tools a Linux host has.
# Bash 3.2, macOS's own, runs it. Needs scripts/lib/user-cache.sh and scripts/lib/containers.sh.

# E2E_MACOS_VOLUME is the volume's name, and so its mount point's, /Volumes/NAME.
E2E_MACOS_VOLUME=cladefs-e2e

# e2e_macos_volume prints the mount point of a case-sensitive APFS volume for the run's results,
# attaching it, made first if it isn't, a sparse disk image in the user's cache that grows as it is
# written (hdiutil; no root): a Mac's own volumes are case-insensitive, where a Linux root's names
# collide (Fedora's terminfo/E/Eterm and terminfo/e/eterm), and the runner refuses to extract one
# there (guest/tests rootfs.rs). It stays attached for the next run, as a cache directory does;
# `hdiutil detach /Volumes/cladefs-e2e` lets it go, and removing the image frees its space.
e2e_macos_volume() {
    local mount=/Volumes/$E2E_MACOS_VOLUME image
    if [[ ! -d $mount ]]; then
        image=$(user_cache)/$E2E_MACOS_VOLUME.sparseimage || return
        if [[ ! -f $image ]]; then
            hdiutil create -quiet -size 1t -type SPARSE -fs 'Case-sensitive APFS' \
                -volname "$E2E_MACOS_VOLUME" "$image" >&2 || return
        fi
        hdiutil attach -quiet -nobrowse "$image" >&2 || return
    fi
    e2e_macos_case_sensitive "$mount" || return
    echo "$mount"
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
