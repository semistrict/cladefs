// Copyright The libkrun Authors.
// Copyright 2026 The cladefs Authors: changed for cladefs's guest tests.
// SPDX-License-Identifier: Apache-2.0

use anyhow::Context;
use std::env::args;
use test_cases::{TestCase, test_cases};

fn run_guest_agent(test_name: &str) -> anyhow::Result<()> {
    let tests = test_cases();
    let test_case = tests.into_iter().find(|t| t.name() == test_name).context("No such test!")?;
    let TestCase { test, .. } = test_case;
    test.in_guest();
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let test_name = args().nth(1).context("Missing test name argument")?;
    // The guest's init, under cladefs's Firecracker, which runs the test and resets the VM, whatever
    // becomes of the test.
    #[cfg(target_os = "linux")]
    if std::process::id() == 1 {
        let init = test_cases::guest_init::set_up();
        if let Ok(Err(err)) = std::panic::catch_unwind(|| run_guest_agent(&test_name)) {
            eprintln!("{err:#}");
        }
        init.reset();
    }
    run_guest_agent(&test_name)?;
    Ok(())
}
