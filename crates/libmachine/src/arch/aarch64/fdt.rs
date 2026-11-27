// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The arm64 machine's device tree: what the kernel finds the machine by, as the Devicetree
//! Specification and Linux's bindings (Documentation/devicetree/bindings) describe each node.
//! The board is a GICv3 with a GICv2m MSI frame (what Hypervisor.framework emulates), the
//! architected timers, PSCI over HVC, a 16550, and a generic ECAM PCI host whose devices
//! interrupt by MSI-X alone.

use vm_fdt::FdtWriter;

use super::layout::{
    GIC_DIST_ADDR, GIC_MSI_ADDR, GIC_REDIST_ADDR, GIC_REDIST_SIZE, MSI_SPI_BASE, MSI_SPI_COUNT,
    PCI_ECAM_ADDR, PCI_ECAM_LAST_BUS, PCI_MMIO32_ADDR, PCI_MMIO32_SIZE, PCI_MMIO64_ADDR,
    PCI_MMIO64_SIZE, RAM_ADDR, RTC_ADDR, RTC_SIZE, RTC_SPI, SPI_BASE, TIMER_PPI_HYP,
    TIMER_PPI_PHYS, TIMER_PPI_SECURE_PHYS, TIMER_PPI_VIRT, UART_ADDR, UART_SIZE, UART_SPI,
};
use crate::error::{Error, Result};

const GIC_PHANDLE: u32 = 1;
const MSI_PHANDLE: u32 = 2;
/// The fixed clock AMBA devices (the RTC) name as their bus clock.
const APB_PCLK_PHANDLE: u32 = 3;
/// What that clock says it runs at: AMBA asks for one, the RTC uses none.
const APB_PCLK_HZ: u32 = 24_000_000;

/// The interrupt specifier's first cell: an SPI or a PPI.
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
/// The GIC's maintenance interrupt, a PPI by INTID, which the framework raises with EL2
/// (`HV_GIC_INT_MAINTENANCE`).
const GIC_MAINTENANCE_PPI: u32 = 25;
/// PCI's address cells' space codes (IEEE 1275 PCI bus binding): 32-bit and 64-bit memory.
const PCI_SPACE_MEM32: u32 = 0x0200_0000;
const PCI_SPACE_MEM64: u32 = 0x0300_0000;
/// What the 16550's divisor counts from.
const UART_CLOCK_HZ: u32 = 1_843_200;

/// What the device tree describes of a machine.
pub(crate) struct Board<'a> {
    pub(crate) vcpus: u32,
    pub(crate) mem_size: u64,
    pub(crate) cmdline: &'a str,
    /// The initramfs: (start, end).
    pub(crate) initrd: Option<(u64, u64)>,
    /// The GIC distributor's size and the MSI frame's, as the framework gives them.
    pub(crate) gic_dist_size: u64,
    pub(crate) gic_msi_size: u64,
    /// Entropy for the kernel's RNG before any driver (`/chosen/rng-seed`).
    pub(crate) rng_seed: [u8; 64],
    /// Whether the vCPUs have EL2 (nested virtualization): the GIC then raises its maintenance
    /// interrupt, which the guest's hypervisor takes.
    pub(crate) el2: bool,
}

fn fdt_err(e: vm_fdt::Error) -> Error {
    Error::InvalidConfig(format!("the device tree: {e}"))
}

/// The machine's device tree, as a flattened blob.
pub(crate) fn device_tree(board: &Board<'_>) -> Result<Vec<u8>> {
    build(board).map_err(fdt_err)
}

fn build(b: &Board<'_>) -> std::result::Result<Vec<u8>, vm_fdt::Error> {
    let mut fdt = FdtWriter::new()?;
    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "libmachine,arm64")?;
    fdt.property_string("model", "libmachine")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;

    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", b.cmdline)?;
    fdt.property_string("stdout-path", &format!("/uart@{UART_ADDR:x}"))?;
    if let Some((start, end)) = b.initrd {
        fdt.property_u64("linux,initrd-start", start)?;
        fdt.property_u64("linux,initrd-end", end)?;
    }
    fdt.property("rng-seed", &b.rng_seed)?;
    fdt.end_node(chosen)?;

    let memory = fdt.begin_node(&format!("memory@{RAM_ADDR:x}"))?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[RAM_ADDR, b.mem_size])?;
    fdt.end_node(memory)?;

    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 1)?;
    fdt.property_u32("#size-cells", 0)?;
    for i in 0..b.vcpus {
        let mpidr = crate::backend::hvf::mpidr(i) as u32;
        let cpu = fdt.begin_node(&format!("cpu@{mpidr:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,arm-v8")?;
        fdt.property_u32("reg", mpidr)?;
        fdt.property_string("enable-method", "psci")?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    let psci = fdt.begin_node("psci")?;
    fdt.property_string_list("compatible", vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()])?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;

    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_null("always-on")?;
    let ppis: Vec<u32> = [TIMER_PPI_SECURE_PHYS, TIMER_PPI_PHYS, TIMER_PPI_VIRT, TIMER_PPI_HYP]
        .iter()
        .flat_map(|&intid| [GIC_PPI, intid - 16, IRQ_TYPE_LEVEL_HIGH])
        .collect();
    fdt.property_array_u32("interrupts", &ppis)?;
    fdt.end_node(timer)?;

    let gic = fdt.begin_node(&format!("intc@{GIC_DIST_ADDR:x}"))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_null("ranges")?;
    fdt.property_array_u64(
        "reg",
        &[GIC_DIST_ADDR, b.gic_dist_size, GIC_REDIST_ADDR, u64::from(b.vcpus) * GIC_REDIST_SIZE],
    )?;
    fdt.property_u32("phandle", GIC_PHANDLE)?;
    if b.el2 {
        fdt.property_array_u32(
            "interrupts",
            &[GIC_PPI, GIC_MAINTENANCE_PPI - 16, IRQ_TYPE_LEVEL_HIGH],
        )?;
    }
    let msi = fdt.begin_node(&format!("v2m@{GIC_MSI_ADDR:x}"))?;
    fdt.property_string("compatible", "arm,gic-v2m-frame")?;
    fdt.property_null("msi-controller")?;
    fdt.property_array_u64("reg", &[GIC_MSI_ADDR, b.gic_msi_size])?;
    // Said, so that the kernel need not read the frame's MSI_TYPER.
    fdt.property_u32("arm,msi-base-spi", MSI_SPI_BASE)?;
    fdt.property_u32("arm,msi-num-spis", MSI_SPI_COUNT)?;
    fdt.property_u32("phandle", MSI_PHANDLE)?;
    fdt.end_node(msi)?;
    fdt.end_node(gic)?;

    let uart = fdt.begin_node(&format!("uart@{UART_ADDR:x}"))?;
    fdt.property_string("compatible", "ns16550a")?;
    fdt.property_array_u64("reg", &[UART_ADDR, UART_SIZE])?;
    fdt.property_u32("clock-frequency", UART_CLOCK_HZ)?;
    fdt.property_array_u32("interrupts", &[GIC_SPI, UART_SPI - SPI_BASE, IRQ_TYPE_EDGE_RISING])?;
    fdt.end_node(uart)?;

    let clock = fdt.begin_node("apb-pclk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0)?;
    fdt.property_u32("clock-frequency", APB_PCLK_HZ)?;
    fdt.property_string("clock-output-names", "clk24mhz")?;
    fdt.property_u32("phandle", APB_PCLK_PHANDLE)?;
    fdt.end_node(clock)?;

    let rtc = fdt.begin_node(&format!("rtc@{RTC_ADDR:x}"))?;
    fdt.property_string_list("compatible", vec!["arm,pl031".into(), "arm,primecell".into()])?;
    fdt.property_array_u64("reg", &[RTC_ADDR, RTC_SIZE])?;
    fdt.property_array_u32("interrupts", &[GIC_SPI, RTC_SPI - SPI_BASE, IRQ_TYPE_LEVEL_HIGH])?;
    fdt.property_u32("clocks", APB_PCLK_PHANDLE)?;
    fdt.property_string("clock-names", "apb_pclk")?;
    fdt.end_node(rtc)?;

    let pci = fdt.begin_node(&format!("pcie@{PCI_ECAM_ADDR:x}"))?;
    fdt.property_string("compatible", "pci-host-ecam-generic")?;
    fdt.property_string("device_type", "pci")?;
    // The buses described: bus 0's 1 MiB of the reserved window.
    let described = (u64::from(PCI_ECAM_LAST_BUS) + 1) << 20;
    fdt.property_array_u64("reg", &[PCI_ECAM_ADDR, described])?;
    fdt.property_array_u32("bus-range", &[0, u32::from(PCI_ECAM_LAST_BUS)])?;
    fdt.property_u32("linux,pci-domain", 0)?;
    fdt.property_u32("#address-cells", 3)?;
    fdt.property_u32("#size-cells", 2)?;
    let window = |space: u32, addr: u64, size: u64| {
        let (hi, lo) = ((addr >> 32) as u32, addr as u32);
        [space, hi, lo, hi, lo, (size >> 32) as u32, size as u32]
    };
    let ranges: Vec<u32> = window(PCI_SPACE_MEM32, PCI_MMIO32_ADDR, PCI_MMIO32_SIZE)
        .into_iter()
        .chain(window(PCI_SPACE_MEM64, PCI_MMIO64_ADDR, PCI_MMIO64_SIZE))
        .collect();
    fdt.property_array_u32("ranges", &ranges)?;
    fdt.property_u32("msi-parent", MSI_PHANDLE)?;
    fdt.property_null("dma-coherent")?;
    fdt.end_node(pci)?;

    fdt.end_node(root)?;
    fdt.finish()
}
