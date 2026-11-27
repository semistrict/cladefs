// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's helper process (`machined::helper`): machined starts one per machine, its VM in
//! it, and speaks to it over its standard input. Not for running by hand.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> std::io::Result<()> {
    machined::helper::main()
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("machined-vm runs on arm64 macOS (Hypervisor.framework)");
    std::process::exit(1);
}
