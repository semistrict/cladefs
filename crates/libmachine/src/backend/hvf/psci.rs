// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! PSCI 1.0 over HVC (Arm DEN 0022), what the guest calls to power vCPUs on and off and the
//! machine off or reset: the device tree's `/psci` says `method = "hvc"`. SMCCC's calling
//! convention: the function in w0, arguments in x1 to x3, the result in x0.

use std::sync::Arc;

use super::kick::CpuSlot;
use super::mpidr;
use crate::machine::ExitReason;

const PSCI_VERSION: u32 = 0x8400_0000;
const CPU_OFF: u32 = 0x8400_0002;
const CPU_ON_32: u32 = 0x8400_0003;
const CPU_ON_64: u32 = 0xc400_0003;
const AFFINITY_INFO_32: u32 = 0x8400_0004;
const AFFINITY_INFO_64: u32 = 0xc400_0004;
const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
const SYSTEM_OFF: u32 = 0x8400_0008;
const SYSTEM_RESET: u32 = 0x8400_0009;
const PSCI_FEATURES: u32 = 0x8400_000a;
/// SMCCC_VERSION, which Linux asks before anything else of SMCCC 1.1.
const SMCCC_VERSION: u32 = 0x8000_0000;

const SUCCESS: i64 = 0;
const NOT_SUPPORTED: i64 = -1;
const INVALID_PARAMETERS: i64 = -2;
const ALREADY_ON: i64 = -4;
/// MIGRATE_INFO_TYPE: no Trusted OS, nothing to migrate.
const NO_TRUSTED_OS: i64 = 2;

/// A guest's HVC: its function and arguments.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Call {
    pub(crate) function: u32,
    pub(crate) args: [u64; 3],
}

/// What the vCPU does after a call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Returns `value` in x0 and runs on.
    Return(i64),
    /// Powers itself off (CPU_OFF): it waits until powered on again.
    Off,
    /// Ends the run: the machine powers off or resets.
    Machine(ExitReason),
}

/// Handles `call` over the machine's vCPUs.
pub(crate) fn handle(call: &Call, cpus: &[Arc<CpuSlot>]) -> Outcome {
    // A vCPU by its MPIDR's affinity fields.
    let target = |mpidr_arg: u64| {
        let affinity = mpidr_arg & 0xff_00ff_ffff;
        (0..cpus.len()).find(|&i| mpidr(i as u32) == affinity).map(|i| &cpus[i])
    };
    match call.function {
        // PSCI 1.0: major 1, minor 0.
        PSCI_VERSION => Outcome::Return(0x1_0000),
        // SMCCC 1.1.
        SMCCC_VERSION => Outcome::Return(0x1_0001),
        PSCI_FEATURES => Outcome::Return(match call.args[0] as u32 {
            PSCI_VERSION | CPU_OFF | CPU_ON_32 | CPU_ON_64 | AFFINITY_INFO_32
            | AFFINITY_INFO_64 | MIGRATE_INFO_TYPE | SYSTEM_OFF | SYSTEM_RESET | PSCI_FEATURES => {
                SUCCESS
            }
            _ => NOT_SUPPORTED,
        }),
        CPU_ON_32 | CPU_ON_64 => {
            let [mpidr_arg, entry, context] = call.args;
            Outcome::Return(match target(mpidr_arg) {
                None => INVALID_PARAMETERS,
                Some(cpu) if cpu.power_on(entry, context) => SUCCESS,
                Some(_) => ALREADY_ON,
            })
        }
        CPU_OFF => Outcome::Off,
        AFFINITY_INFO_32 | AFFINITY_INFO_64 => {
            let [mpidr_arg, level, _] = call.args;
            Outcome::Return(match target(mpidr_arg) {
                _ if level != 0 => INVALID_PARAMETERS,
                None => INVALID_PARAMETERS,
                // ON 0, OFF 1.
                Some(cpu) => i64::from(!cpu.is_on()),
            })
        }
        MIGRATE_INFO_TYPE => Outcome::Return(NO_TRUSTED_OS),
        SYSTEM_OFF => Outcome::Machine(ExitReason::Poweroff),
        SYSTEM_RESET => Outcome::Machine(ExitReason::Reset),
        _ => Outcome::Return(NOT_SUPPORTED),
    }
}
