// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The PC platform's ACPI tables: a hardware-reduced FADT whose sleep and
//! reset registers power the machine off and reset it, a DSDT naming the
//! soft-off state and describing the devices (COM1, the PCI host bridge), a
//! MADT listing the local APICs and the I/O APIC, and an MCFG locating the
//! PCI configuration space (ECAM).

use acpi_tables::Aml;
use acpi_tables::aml::{
    self, AddressSpaceCacheable, Arg, Device, EISAName, Equal, IO, If, Interrupt, Memory32Fixed,
    Method, Name, Notify, Package, Path, ResourceTemplate, Scope,
};
use acpi_tables::fadt::{FADTBuilder, Flags};
use acpi_tables::gas::{AccessSize, AddressSpace, GAS};
use acpi_tables::mcfg::MCFG;
use acpi_tables::rsdp::Rsdp;
use acpi_tables::sdt::Sdt;
use acpi_tables::xsdt::XSDT;
use zerocopy::IntoBytes;

use super::layout::{
    ACPI_ADDR, ACPI_END, ACPI_RESET_PORT, ACPI_RESET_VALUE, ACPI_S5_SLP_TYP,
    ACPI_SLEEP_CONTROL_PORT, ACPI_SLEEP_STATUS_PORT, COM1_IRQ, GED_IRQ, IOAPIC_ADDR, LAPIC_ADDR,
    PCI_ECAM_ADDR, PCI_ECAM_LAST_BUS, PCI_ECAM_SIZE, PCI_MMIO32_ADDR, PCI_MMIO32_SIZE,
    PCI_MMIO64_ADDR, PCI_MMIO64_SIZE, VMGENID_ADDR,
};

/// COM1's I/O ports.
const COM1_PORT: u16 = 0x3f8;

const OEM_ID: [u8; 6] = *b"LIBMCH";
const OEM_TABLE_ID: [u8; 8] = *b"LIBMACHN";
const OEM_REVISION: u32 = 1;

/// FADT IAPC_BOOT_ARCH: no VGA, no CMOS RTC (and, by omission, no 8042
/// keyboard controller and no legacy devices beyond the UART).
const IAPC_BOOT_ARCH_NO_VGA: u16 = 1 << 2;
const IAPC_BOOT_ARCH_NO_CMOS_RTC: u16 = 1 << 5;

/// MADT flags: the machine also has dual 8259 PICs (KVM's irqchip has them).
const MADT_PCAT_COMPAT: u32 = 1;

/// The tables, each with the guest physical address it goes at. The RSDP is
/// first, at `ACPI_ADDR`.
pub(crate) fn tables(vcpu_count: u32) -> Vec<(u64, Vec<u8>)> {
    let mut next = ACPI_ADDR + 64; // the RSDP's slot
    let mut place = |len: usize| {
        let addr = next;
        next = (next + len as u64).next_multiple_of(16);
        assert!(
            next <= VMGENID_ADDR && VMGENID_ADDR + 16 <= ACPI_END,
            "ACPI tables overflow their area"
        );
        addr
    };

    let dsdt = dsdt();
    let dsdt_addr = place(dsdt.len());
    let fadt = fadt(dsdt_addr);
    let fadt_addr = place(fadt.len());
    let madt = madt(vcpu_count);
    let madt_addr = place(madt.len());
    let mcfg = mcfg();
    let mcfg_addr = place(mcfg.len());

    let mut xsdt = XSDT::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    xsdt.add_entry(fadt_addr);
    xsdt.add_entry(madt_addr);
    xsdt.add_entry(mcfg_addr);
    let xsdt = aml_bytes(&xsdt);
    let xsdt_addr = place(xsdt.len());

    let rsdp = Rsdp::new(OEM_ID, xsdt_addr).as_bytes().to_vec();
    vec![
        (ACPI_ADDR, rsdp),
        (xsdt_addr, xsdt),
        (fadt_addr, fadt),
        (dsdt_addr, dsdt),
        (madt_addr, madt),
        (mcfg_addr, mcfg),
    ]
}

fn aml_bytes(aml: &dyn Aml) -> Vec<u8> {
    let mut bytes = Vec::new();
    aml.to_aml_bytes(&mut bytes);
    bytes
}

fn io_register(port: u16) -> GAS {
    GAS {
        address_space_id: AddressSpace::SystemIo,
        register_bit_width: 8,
        register_bit_offset: 0,
        access_size: AccessSize::ByteAccess,
        address: u64::from(port).into(),
    }
}

fn fadt(dsdt_addr: u64) -> Vec<u8> {
    let mut fadt = FADTBuilder::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION)
        .flag(Flags::HwReducedAcpi)
        .flag(Flags::ResetRegSup)
        // No fixed power or sleep buttons.
        .flag(Flags::PwrButton)
        .flag(Flags::SlpButton)
        .dsdt_64(dsdt_addr);
    fadt.iapc_boot_arch = (IAPC_BOOT_ARCH_NO_VGA | IAPC_BOOT_ARCH_NO_CMOS_RTC).into();
    fadt.reset_reg = io_register(ACPI_RESET_PORT);
    fadt.reset_value = ACPI_RESET_VALUE;
    fadt.sleep_control_reg = io_register(ACPI_SLEEP_CONTROL_PORT);
    fadt.sleep_status_reg = io_register(ACPI_SLEEP_STATUS_PORT);
    fadt.hypervisor_vendor_identity = u64::from_le_bytes(*b"LIBMACHN").into();
    aml_bytes(&fadt.finalize())
}

/// The DSDT:
///
/// ```text
/// Scope (\_SB) {
///     Device (COM1) {           // the console UART
///         Name (_HID, EisaId ("PNP0501"))
///         Name (_UID, 0)
///         Name (_CRS, ResourceTemplate () {
///             IO (Decode16, 0x3F8, 0x3F8, 0, 8)
///             Interrupt (ResourceConsumer, Edge, ActiveHigh, Exclusive) { 4 }
///         })
///     }
///     Device (PCI0) {           // the PCI host bridge: segment 0, bus 0
///         Name (_HID, EisaId ("PNP0A03"))
///         Name (_UID, 0)
///         Name (_SEG, 0)
///         Name (_BBN, 0)
///         Name (_CRS, ResourceTemplate () {
///             WordBusNumber (ResourceProducer, ..., 0, 0, ...)
///             DWordMemory (ResourceProducer, ..., NonCacheable, ReadWrite,
///                          0xC0000000, 0xDFFFFFFF)    // 32-bit BARs
///             QWordMemory (ResourceProducer, ..., NonCacheable, ReadWrite,
///                          0x10000000000, 0x1FFFFFFFFFF)  // 64-bit BARs
///         })
///     }
///     Device (MRES) {           // motherboard resources: the ECAM window
///         Name (_HID, EisaId ("PNP0C02"))
///         Name (_UID, 0)
///         Name (_CRS, ResourceTemplate () {
///             Memory32Fixed (ReadWrite, 0xE0000000, 0x10000000)
///         })
///     }
///     Device (VGEN) {           // the VM generation ID
///         Name (_HID, "VMGENCTR")
///         Name (_UID, 0)
///         Name (_CID, "VM_Gen_Counter")
///         Name (_DDN, "VM_Gen_Counter")
///         Name (ADDR, Package () { 0xFF000, 0 })
///     }
///     Device (GED0) {           // its change event
///         Name (_HID, "ACPI0013")
///         Name (_UID, 0)
///         Name (_CRS, ResourceTemplate () {
///             Interrupt (ResourceConsumer, Edge, ActiveHigh, Exclusive) { 16 }
///         })
///         Method (_EVT, 1, Serialized) {
///             If (Arg0 == 16) { Notify (\_SB.VGEN, 0x80) }
///         }
///     }
/// }
/// Name (_S5_, Package () { 5, 5, 0, 0 })  // soft-off sleep type
/// ```
///
/// A hardware-reduced platform has no legacy PIC, so the OS only finds the
/// UART's interrupt through this description. The host bridge is
/// conventional PCI (PNP0A03): its devices have no PCI Express capability,
/// and their interrupts are MSI-X, so it needs no `_PRT` and no `_OSC`. The
/// ECAM is reserved as a motherboard resource, as PCI Firmware 3.3 section
/// 4.1.2 requires and Linux checks before it uses the MCFG's ECAM.
fn dsdt() -> Vec<u8> {
    let mut dsdt = Sdt::new(*b"DSDT", 36, 6, OEM_ID, OEM_TABLE_ID, OEM_REVISION);

    let hid = EISAName::new("PNP0501");
    let uid = 0u8;
    let io = IO::new(COM1_PORT, COM1_PORT, 0, 8);
    let irq = Interrupt::new(true, true, false, false, COM1_IRQ);
    let crs = ResourceTemplate::new(vec![&io, &irq]);
    let hid = Name::new(Path::new("_HID"), &hid);
    let uid = Name::new(Path::new("_UID"), &uid);
    let crs = Name::new(Path::new("_CRS"), &crs);
    let com1 = Device::new(Path::new("COM1"), vec![&hid, &uid, &crs]);

    let hid = EISAName::new("PNP0A03");
    let hid = Name::new(Path::new("_HID"), &hid);
    let seg = Name::new(Path::new("_SEG"), &aml::ZERO);
    let bbn = Name::new(Path::new("_BBN"), &aml::ZERO);
    let buses = aml::AddressSpace::new_bus_number(0u16, u16::from(PCI_ECAM_LAST_BUS));
    let mmio32 = aml::AddressSpace::new_memory(
        AddressSpaceCacheable::NotCacheable,
        true,
        PCI_MMIO32_ADDR as u32,
        (PCI_MMIO32_ADDR + PCI_MMIO32_SIZE - 1) as u32,
        None,
    );
    let mmio64 = aml::AddressSpace::new_memory(
        AddressSpaceCacheable::NotCacheable,
        true,
        PCI_MMIO64_ADDR,
        PCI_MMIO64_ADDR + PCI_MMIO64_SIZE - 1,
        None,
    );
    let crs = ResourceTemplate::new(vec![&buses, &mmio32, &mmio64]);
    let crs = Name::new(Path::new("_CRS"), &crs);
    let pci0 = Device::new(Path::new("PCI0"), vec![&hid, &uid, &seg, &bbn, &crs]);

    let hid = EISAName::new("PNP0C02");
    let hid = Name::new(Path::new("_HID"), &hid);
    let ecam = Memory32Fixed::new(true, PCI_ECAM_ADDR as u32, PCI_ECAM_SIZE as u32);
    let crs = ResourceTemplate::new(vec![&ecam]);
    let crs = Name::new(Path::new("_CRS"), &crs);
    let mres = Device::new(Path::new("MRES"), vec![&hid, &uid, &crs]);

    // The VM generation ID (Microsoft's VMGENID, which Linux's vmgenid driver
    // binds by its _CID): ADDR is where its 16 bytes are, as two halves.
    let hid = Name::new(Path::new("_HID"), &"VMGENCTR");
    let cid = Name::new(Path::new("_CID"), &"VM_Gen_Counter");
    let ddn = Name::new(Path::new("_DDN"), &"VM_Gen_Counter");
    let (low, high) = (VMGENID_ADDR as u32, (VMGENID_ADDR >> 32) as u32);
    let addr = Package::new(vec![&low, &high]);
    let addr = Name::new(Path::new("ADDR"), &addr);
    let vgen = Device::new(Path::new("VGEN"), vec![&hid, &uid, &cid, &ddn, &addr]);

    // The Generic Event Device of a hardware-reduced platform (ACPI 6.5,
    // section 5.6.9): its interrupt runs _EVT, which tells the OS that the
    // generation ID changed (Notify 0x80, as the VMGENID spec has it).
    let hid = Name::new(Path::new("_HID"), &"ACPI0013");
    let irq = Interrupt::new(true, true, false, false, GED_IRQ);
    let crs = ResourceTemplate::new(vec![&irq]);
    let crs = Name::new(Path::new("_CRS"), &crs);
    let vgen_path = Path::new("\\_SB_.VGEN");
    let notify = Notify::new(&vgen_path, &0x80u8);
    let ged_irq = GED_IRQ;
    let is_ours = Equal::new(&Arg(0), &ged_irq);
    let on_irq = If::new(&is_ours, vec![&notify]);
    let evt = Method::new(Path::new("_EVT"), 1, true, vec![&on_irq]);
    let ged = Device::new(Path::new("GED0"), vec![&hid, &uid, &crs, &evt]);

    let sb = Scope::new(Path::new("\\_SB_"), vec![&com1, &pci0, &mres, &vgen, &ged]);
    dsdt.append_slice(&aml_bytes(&sb));

    let s5 = ACPI_S5_SLP_TYP;
    let package = Package::new(vec![&s5, &s5, &0u8, &0u8]);
    dsdt.append_slice(&aml_bytes(&Name::new(Path::new("_S5_"), &package)));
    dsdt.as_slice().to_vec()
}

/// The MCFG: the ECAM of segment 0, buses 0 to `PCI_ECAM_LAST_BUS` (0).
fn mcfg() -> Vec<u8> {
    let mut mcfg = MCFG::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    mcfg.add_ecam(PCI_ECAM_ADDR, 0, 0, PCI_ECAM_LAST_BUS);
    aml_bytes(&mcfg)
}

fn madt(vcpu_count: u32) -> Vec<u8> {
    let mut madt = Sdt::new(*b"APIC", 44, 5, OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    madt.write_u32(36, LAPIC_ADDR);
    madt.write_u32(40, MADT_PCAT_COMPAT);
    for cpu in 0..vcpu_count {
        // Processor Local APIC: type 0, length 8, ACPI UID, APIC ID, enabled.
        let id = u8::try_from(cpu).expect("xAPIC IDs fit in a byte");
        madt.append_slice(&[0, 8, id, id]);
        madt.append_slice(&1u32.to_le_bytes());
    }
    // I/O APIC: type 1, length 12, ID (after the CPUs'), address, GSI base 0.
    let ioapic_id = u8::try_from(vcpu_count).expect("I/O APIC ID fits in a byte");
    madt.append_slice(&[1, 12, ioapic_id, 0]);
    madt.append_slice(&IOAPIC_ADDR.to_le_bytes());
    madt.append_slice(&0u32.to_le_bytes());
    // Interrupt Source Override: ISA IRQ 0 (the PIT) is wired to GSI 2.
    madt.append_slice(&[2, 10, 0, 0]);
    madt.append_slice(&2u32.to_le_bytes());
    madt.append_slice(&0u16.to_le_bytes());
    // Local APIC NMI: every processor's LINT1, as on PCs.
    madt.append_slice(&[4, 6, 0xff]);
    madt.append_slice(&0u16.to_le_bytes());
    madt.append_slice(&[1]);
    madt.as_slice().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0u8, |s, b| s.wrapping_add(*b))
    }

    fn table<'a>(tables: &'a [(u64, Vec<u8>)], signature: &[u8]) -> &'a [u8] {
        &tables
            .iter()
            .find(|(_, t)| t.starts_with(signature))
            .unwrap_or_else(|| panic!("{} missing", String::from_utf8_lossy(signature)))
            .1
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn every_table_checksums_to_zero_and_has_its_length() {
        let tables = tables(1);
        let rsdp = table(&tables, b"RSD PTR ");
        assert_eq!(sum(&rsdp[..20]), 0, "RSDP v1 checksum");
        assert_eq!(sum(rsdp), 0, "RSDP extended checksum");
        for (_, t) in &tables[1..] {
            let name = String::from_utf8_lossy(&t[..4]).into_owned();
            assert_eq!(u32_at(t, 4) as usize, t.len(), "{name} length");
            assert_eq!(sum(t), 0, "{name} checksum");
        }
    }

    #[test]
    fn tables_point_at_each_other() {
        let tables = tables(2);
        let addr_of = |sig: &[u8]| tables.iter().find(|(_, t)| t.starts_with(sig)).unwrap().0;
        let rsdp = table(&tables, b"RSD PTR ");
        assert_eq!(u64_at(rsdp, 24), addr_of(b"XSDT"));
        let xsdt = table(&tables, b"XSDT");
        assert_eq!(u64_at(xsdt, 36), addr_of(b"FACP"));
        assert_eq!(u64_at(xsdt, 44), addr_of(b"APIC"));
        assert_eq!(u64_at(xsdt, 52), addr_of(b"MCFG"));
        let fadt = table(&tables, b"FACP");
        assert_eq!(u64_at(fadt, 140), addr_of(b"DSDT"), "X_DSDT");
        for (addr, t) in &tables {
            assert!(*addr >= ACPI_ADDR && addr + t.len() as u64 <= ACPI_END);
        }
    }

    #[test]
    fn fadt_is_hardware_reduced_with_sleep_and_reset_registers() {
        let tables = tables(1);
        let fadt = table(&tables, b"FACP");
        let flags = u32_at(fadt, 112);
        assert_ne!(flags & Flags::HwReducedAcpi as u32, 0);
        assert_ne!(flags & Flags::ResetRegSup as u32, 0);
        // RESET_REG (GAS at 116): system I/O, the reset port; RESET_VALUE.
        assert_eq!(fadt[116], 1);
        assert_eq!(u64_at(fadt, 120), u64::from(ACPI_RESET_PORT));
        assert_eq!(fadt[128], ACPI_RESET_VALUE);
        // SLEEP_CONTROL_REG (GAS at 244) and SLEEP_STATUS_REG (at 256).
        assert_eq!(u64_at(fadt, 248), u64::from(ACPI_SLEEP_CONTROL_PORT));
        assert_eq!(u64_at(fadt, 260), u64::from(ACPI_SLEEP_STATUS_PORT));
    }

    #[test]
    fn dsdt_names_s5_and_describes_com1() {
        let tables = tables(1);
        let dsdt = table(&tables, b"DSDT");
        for name in [&b"_S5_"[..], b"_SB_", b"COM1", b"_HID", b"_CRS"] {
            assert!(
                dsdt.windows(4).any(|w| w == name),
                "{} missing",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// The generation ID device Linux's vmgenid binds (by _CID), with its
    /// address, and the event device whose interrupt notifies it.
    #[test]
    fn dsdt_describes_the_vm_generation_id_and_its_event() {
        let tables = tables(1);
        let dsdt = table(&tables, b"DSDT");
        let has = |bytes: &[u8]| dsdt.windows(bytes.len()).any(|w| w == bytes);
        for name in [&b"VGEN"[..], b"VM_Gen_Counter\0", b"ADDR", b"GED0", b"ACPI0013\0", b"_EVT"] {
            assert!(has(name), "{} missing", String::from_utf8_lossy(name));
        }
        // ADDR: a package of two DWords, the address's halves.
        let mut addr = vec![0x0c];
        addr.extend_from_slice(&(VMGENID_ADDR as u32).to_le_bytes());
        assert!(has(&addr), "ADDR's low half");
        // The GED's interrupt: an extended IRQ descriptor, consumer, edge,
        // active high, exclusive, GSI 16.
        let mut irq = vec![0x89, 6, 0, 0b11, 1];
        irq.extend_from_slice(&GED_IRQ.to_le_bytes());
        assert!(has(&irq), "GED interrupt");
        // Notify (\_SB.VGEN, 0x80).
        assert!(has(&[0x86, b'\\', 0x2e, b'_', b'S', b'B', b'_', b'V', b'G', b'E', b'N']));
        // Nothing overlaps the generation ID.
        for (addr, t) in &tables {
            assert!(addr + t.len() as u64 <= VMGENID_ADDR);
        }
    }

    #[test]
    fn dsdt_describes_the_pci_host_bridge_and_reserves_the_ecam() {
        let tables = tables(1);
        let dsdt = table(&tables, b"DSDT");
        let has = |bytes: &[u8]| dsdt.windows(bytes.len()).any(|w| w == bytes);
        for name in [&b"PCI0"[..], b"_SEG", b"_BBN", b"MRES"] {
            assert!(has(name), "{} missing", String::from_utf8_lossy(name));
        }
        // EISA IDs, compressed: PNP0A03 (PCI host bridge), PNP0C02.
        assert!(has(&[0x41, 0xd0, 0x0a, 0x03]), "PNP0A03");
        assert!(has(&[0x41, 0xd0, 0x0c, 0x02]), "PNP0C02");
        // WordBusNumber 0-0: descriptor 0x88, length 13, type 2 (bus),
        // flags, granularity 0, min 0, max 0, translation 0, length 1.
        assert!(has(&[0x88, 0x0d, 0x00, 0x02, 0x0c, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]));
        // The 32-bit window: min, max.
        let mut window32 = (PCI_MMIO32_ADDR as u32).to_le_bytes().to_vec();
        window32.extend_from_slice(&((PCI_MMIO32_ADDR + PCI_MMIO32_SIZE - 1) as u32).to_le_bytes());
        assert!(has(&window32), "32-bit window");
        // The 64-bit window: min, max.
        let mut window64 = PCI_MMIO64_ADDR.to_le_bytes().to_vec();
        window64.extend_from_slice(&(PCI_MMIO64_ADDR + PCI_MMIO64_SIZE - 1).to_le_bytes());
        assert!(has(&window64), "64-bit window");
        // Memory32Fixed (0x86, length 9, read-write) over the ECAM.
        let mut ecam = vec![0x86, 0x09, 0x00, 0x01];
        ecam.extend_from_slice(&(PCI_ECAM_ADDR as u32).to_le_bytes());
        ecam.extend_from_slice(&(PCI_ECAM_SIZE as u32).to_le_bytes());
        assert!(has(&ecam), "ECAM reservation");
    }

    #[test]
    fn mcfg_locates_the_ecam() {
        let tables = tables(1);
        let mcfg = table(&tables, b"MCFG");
        assert_eq!(mcfg.len(), 36 + 8 + 16, "one allocation");
        assert_eq!(u64_at(mcfg, 44), PCI_ECAM_ADDR);
        assert_eq!(&mcfg[52..56], &[0, 0, 0, 0], "segment 0, bus 0 only");
        // The window holds the buses described: 1 MiB each.
        const { assert!(PCI_ECAM_SIZE >= (PCI_ECAM_LAST_BUS as u64 + 1) << 20) };
    }

    #[test]
    fn madt_lists_every_cpu_and_the_ioapic() {
        let madt_for = |n: u32| table(&tables(n), b"APIC").to_vec();
        for n in [1, 4] {
            let madt = madt_for(n);
            assert_eq!(u32_at(&madt, 36), LAPIC_ADDR);
            let mut off = 44;
            let mut lapic_ids = Vec::new();
            let mut ioapics = 0;
            while off < madt.len() {
                let (kind, len) = (madt[off], madt[off + 1] as usize);
                // A shorter entry would never advance the walk.
                assert!(len >= 2, "MADT entry at {off} has length {len}");
                match kind {
                    0 => lapic_ids.push(madt[off + 3]),
                    1 => {
                        ioapics += 1;
                        assert_eq!(u32_at(&madt, off + 4), IOAPIC_ADDR);
                    }
                    _ => {}
                }
                off += len;
            }
            assert_eq!(off, madt.len(), "entries fill the table exactly");
            assert_eq!(lapic_ids, (0..n as u8).collect::<Vec<_>>());
            assert_eq!(ioapics, 1);
        }
    }
}
