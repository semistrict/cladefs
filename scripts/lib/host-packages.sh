# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The packages of scripts/host-packages.txt, and their install, sourced by scripts/host-packages.sh
# and scripts/lib/linux-host-setup.sh. Sourcing it defines them and does nothing else.

# host_packages prints the packages the list names, one a line.
host_packages() {
    local list
    list="$(dirname "${BASH_SOURCE[0]}")/../host-packages.txt"
    sed 's/#.*//' "$list" | tr -s '[:space:]' '\n' | sed '/^$/d'
}

# host_apt ARG... runs apt-get with ARGs, waiting for its lock, restarting no
# service: needrestart, which a host installs, restarted sshd and re-executed
# systemd after a run's packages went in, and the run's job, which they ran
# under, died with no status 11 s later (GCE r990, 2026-11-08).
host_apt() {
    NEEDRESTART_SUSPEND=1 DEBIAN_FRONTEND=noninteractive \
        apt-get -o DPkg::Lock::Timeout=600 "$@"
}

# host_packages_marker MARKER_DIR prints the path of MARKER_DIR's marker of this list, which names
# it, so that a host that installed a different one installs again.
host_packages_marker() {
    echo "$1/host-packages-$(host_packages | sha256sum | cut -c1-16)"
}

# host_packages_installed MARKER_DIR says whether MARKER_DIR's marker of this list is there.
host_packages_installed() {
    [[ -f $(host_packages_marker "$1") ]]
}

# host_packages_install MARKER_DIR installs the list's packages, as root, unless MARKER_DIR's
# marker of this list says they went in, and leaves the marker.
host_packages_install() {
    local packages marker
    marker=$(host_packages_marker "$1")
    [[ ! -f $marker ]] || return 0
    mapfile -t packages < <(host_packages)
    host_apt update -qq
    host_apt install -y -qq "${packages[@]}"
    mkdir -p "$1"
    touch "$marker"
}
