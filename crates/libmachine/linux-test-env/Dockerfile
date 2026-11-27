# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The root filesystem of the Linux machine libmachine boots on macOS to run the tests that need
# Linux and KVM (crates/libmachine/scripts/linux-test-env.sh builds it; the linux_test_env example
# imports it into a cell of cladefs's daemon and boots a machine with nested virtualization on it).
# The machine has no network: everything the tests fetch is fetched here.
FROM rust:1-bookworm

# The host's packages, as a test host installs them (scripts/host-packages.sh).
COPY work/scripts/host-packages.sh work/scripts/host-packages.txt /work/scripts/
COPY work/scripts/lib/host-packages.sh /work/scripts/lib/
RUN /work/scripts/host-packages.sh /tmp/markers && rm -rf /var/lib/apt/lists/*
RUN curl -LsSf https://get.nexte.st/latest/linux-arm | tar zxf - -C /usr/local/cargo/bin

# The toolchains the workspaces pin, with their targets.
COPY work/rust-toolchain.toml /work/
COPY work/vendor/firecracker/rust-toolchain.toml /work/vendor/firecracker/
COPY work/guest/rust/rust-toolchain.toml /work/guest/rust/
RUN cd /work && rustup show active-toolchain || rustup toolchain install
RUN cd /work/vendor/firecracker && (rustup show active-toolchain || rustup toolchain install)
RUN cd /work/guest/rust && (rustup show active-toolchain || rustup toolchain install)

# The repository, and every crate its workspaces build, fetched.
COPY work /work
RUN cd /work && cargo fetch --locked \
    && cargo fetch --locked --manifest-path guest/tests/Cargo.toml \
    && cargo fetch --locked --manifest-path guest/rust/Cargo.toml \
    && scripts/firecracker-cargo.sh fetch --locked

# libmachine's init and guest agent (crates/libmachine/guest/init.c, crates/libmachine-agent), as
# the machine's init: `init=/sbin/libmachine-init libmachine_action=agent`; and cladefs's guest
# kernel, which the guest tests boot their Firecracker VMs with (CLADEFS_GUEST_KERNEL).
COPY libmachine-init /sbin/libmachine-init
COPY libmachine-agent /sbin/libmachine-agent
COPY Image /opt/cladefs-guest/Image
