// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bazel build script for `//vendor:firecracker` that compiles Firecracker's seccomp JSON
//! policies merged with cladefs's per-thread rules into `$OUT_DIR/seccomp_filter.bpf`,
//! matching `vendor/firecracker/src/firecracker/build.rs` without modifying the submodule.

use std::path::Path;

const ADVANCED_BINARY_FILTER_FILE_NAME: &str = "seccomp_filter.bpf";
const JSON_DIR: &str = "firecracker/resources/seccomp";
const CLADEFS_JSON_DIR: &str = "firecracker/resources/cladefs/seccomp";

fn main() {
    let target = std::env::var("TARGET").expect("Missing TARGET");
    let debug: bool = std::env::var("DEBUG")
        .expect("Missing DEBUG")
        .parse()
        .expect("Invalid DEBUG");
    let out_dir = std::env::var("OUT_DIR").expect("Missing OUT_DIR");
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("Missing CARGO_CFG_TARGET_ARCH");

    let target_json = format!("{}/{}.json", JSON_DIR, target);
    let seccomp_json_path = if debug || !Path::new(&target_json).exists() {
        format!("{}/unimplemented.json", JSON_DIR)
    } else {
        target_json.clone()
    };

    let seccomp_json_path = if seccomp_json_path == target_json {
        with_cladefs_rules(&seccomp_json_path, &target, &out_dir)
    } else {
        seccomp_json_path
    };

    let out_path = format!("{}/{}", out_dir, ADVANCED_BINARY_FILTER_FILE_NAME);
    seccompiler::compile_bpf(&seccomp_json_path, &target_arch, &out_path, false, false)
        .expect("Cannot compile Firecracker seccomp filters");
}

fn with_cladefs_rules(seccomp_json_path: &str, target: &str, out_dir: &str) -> String {
    let read = |path: &str| -> serde_json::Value {
        let json = std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{path}: {err}"));
        serde_json::from_str(&json).unwrap_or_else(|err| panic!("{path}: {err}"))
    };
    let cladefs_json_path = format!("{}/{}.json", CLADEFS_JSON_DIR, target);
    let mut filters = read(seccomp_json_path);
    let cladefs = read(&cladefs_json_path);
    let threads = cladefs
        .as_object()
        .unwrap_or_else(|| panic!("{cladefs_json_path}: not an object of threads' filters"));
    for (thread, rules) in threads {
        let filter = filters[thread.as_str()]["filter"]
            .as_array_mut()
            .unwrap_or_else(|| panic!("{seccomp_json_path}: no filter for thread {thread}"));
        let rules = rules["filter"]
            .as_array()
            .unwrap_or_else(|| panic!("{cladefs_json_path}: no filter for thread {thread}"));
        filter.splice(0..0, rules.iter().cloned());
    }
    let merged = format!("{}/seccomp_filter_cladefs.json", out_dir);
    std::fs::write(&merged, serde_json::to_string_pretty(&filters).unwrap())
        .unwrap_or_else(|err| panic!("{merged}: {err}"));
    merged
}
