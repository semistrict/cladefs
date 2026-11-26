// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The CPU model a guest sees.

/// Which CPU features the guest is shown.
///
/// Whatever the model, the guest sees only the CPUID leaves libmachine
/// lists (a leaf a newer hypervisor adds stays hidden until libmachine
/// decides on it), and libmachine hides features a sandbox must not expose
/// or cannot support: nested virtualization, performance counters, SGX and
/// processor trace. The CPU vendor is always the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuModel {
    /// Every feature KVM supports on this host within the listed leaves,
    /// minus the hidden ones. Fastest, but the model differs between host
    /// CPU generations.
    #[default]
    Host,
    /// A fixed x86-64-v3 feature set (AVX2, BMI1/2, FMA, MOVBE, ...; no
    /// AVX-512). The same on every host that can provide it; creating a
    /// machine fails, naming the missing features, on a host that cannot.
    Baseline,
}

impl std::str::FromStr for CpuModel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "host" => Ok(Self::Host),
            "baseline" => Ok(Self::Baseline),
            other => Err(format!("unknown CPU model {other:?}, expected \"host\" or \"baseline\"")),
        }
    }
}

impl std::fmt::Display for CpuModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Host => "host",
            Self::Baseline => "baseline",
        })
    }
}

/// One CPUID leaf (and subleaf) as the guest sees it.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub struct CpuidEntry {
    /// The leaf (EAX input).
    pub function: u32,
    /// The subleaf (ECX input); only meaningful when `index_significant`.
    pub index: u32,
    /// Whether the result depends on the subleaf.
    pub index_significant: bool,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[cfg(target_arch = "x86_64")]
impl CpuidEntry {
    /// Whether this entry answers CPUID with EAX=`function`, ECX=`index`.
    pub fn matches(&self, function: u32, index: u32) -> bool {
        self.function == function && (!self.index_significant || self.index == index)
    }
}
