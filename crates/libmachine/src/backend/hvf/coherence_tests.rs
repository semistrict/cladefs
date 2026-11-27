// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What Hypervisor.framework does with guest RAM whose host mapping changes under it, which given
//! RAM (`ram::GuestRam`) and a memory cell's window over it (cladefs's `dax::hvf::ram`) rely on
//! (board 13994): a bare guest reads and writes pages while the host maps them anew between its
//! runs, each read reported as an MMIO write's value.
//!
//! - The framework binds a guest page to the host's page as it maps it, and follows no later
//!   `mmap(MAP_FIXED)` of the host's there: the range is mapped into the guest anew after each
//!   ([`GuestRam::rebind`](cladefs::GuestRam::rebind)).
//! - A private (copy-on-write) file mapping, so mapped, stays coherent both ways: the guest's
//!   write copies the page, which the host reads; the host's write after the guest's read copies
//!   it too, which the guest reads. `mincore` says a page so copied is anonymous, not before.

use super::*;
use crate::arch::aarch64::layout::RAM_ADDR;
use dynasmrt::dynasm;

/// Where the guest reports what it read: an address nothing backs.
const REPORT: u64 = 0x0804_0000;
/// `mincore`'s bit for a page of an anonymous object, a copy on write of a file's among them.
const MINCORE_ANONYMOUS: u8 = 0x80;

/// The vCPU's run to its next report: the value it wrote there.
fn report(vcpu: &mut Vcpu) -> u64 {
    loop {
        match vcpu.run_once().expect("run") {
            Exit::MmioWrite { addr, data } => {
                assert_eq!(addr, REPORT);
                return u64::from_le_bytes(data.try_into().expect("a 64-bit store"));
            }
            Exit::Handled(_) | Exit::Interrupted => {}
            Exit::Done(reason) => panic!("the guest stopped: {reason:?}"),
            _ => panic!("an unexpected exit"),
        }
    }
}

/// A file of one host page of `byte`s, mapped privately over `at`, a host page of RAM.
fn map_file_over(at: *mut u8, byte: u8) {
    use std::io::Write as _;
    let page = crate::sys::page_size();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&vec![byte; page]).unwrap();
    // SAFETY: Replaces a page of the VM's RAM, which only this test's guest uses.
    let mapped = unsafe {
        libc::mmap(
            at.cast(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            std::os::fd::AsRawFd::as_raw_fd(&file),
            0,
        )
    };
    assert_eq!(mapped, at.cast());
}

/// Maps the host page of RAM at `host` into the guest at `guest_addr` anew.
fn rebind(host: *mut u8, guest_addr: u64) {
    let page = crate::sys::page_size();
    // SAFETY: A page of the VM's RAM, mapped anew from what the host maps there now.
    unsafe {
        check("unmap", hv_vm_unmap(guest_addr, page)).unwrap();
        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;
        check("map", hv_vm_map(host.cast(), guest_addr, page, flags)).unwrap();
    }
}

fn mincore(at: *mut u8) -> u8 {
    let mut bits = [0u8; 1];
    // SAFETY: One page of this process's, a byte for it.
    let ret =
        unsafe { libc::mincore(at.cast(), crate::sys::page_size(), bits.as_mut_ptr().cast()) };
    assert_eq!(ret, 0);
    bits[0]
}

#[test]
fn guest_ram_follows_its_host_mapping_once_mapped_anew() {
    let (vm, vcpus) = Vm::new(64 << 20, Platform::Bare, 1, Ram::Anonymous).expect("the VM");
    let page = |i: u64| RAM_ADDR + (1 << 20) + i * crate::sys::page_size() as u64;
    let host = |i: u64| vm.guest_memory().get_host_address(GuestAddress(page(i))).unwrap();
    let mut ops =
        dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(RAM_ADDR as usize);
    // x0: the report's address; x1: page A, x2: page B.
    dynasm!(ops
        ; .arch aarch64
        // A, read; read again once a file is mapped over it; again once mapped anew.
        ; ldr x4, [x1]
        ; str x4, [x0]
        ; ldr x4, [x1]
        ; str x4, [x0]
        ; ldr x4, [x1]
        ; str x4, [x0]
        // Again, once the host wrote it.
        ; ldr x4, [x1]
        ; str x4, [x0]
        // B, a file's, read, then written, then read once the host wrote it.
        ; ldr x4, [x2]
        ; str x4, [x0]
        ; movz x5, 0x55
        ; str x5, [x2, 8]
        ; str x5, [x0]
        ; ldr x4, [x2]
        ; str x4, [x0]
        ; b 0
    );
    vm.write_memory(RAM_ADDR, &ops.finalize().unwrap()).unwrap();
    let byte = |b: u8| u64::from_le_bytes([b; 8]);
    // SAFETY: The VM's RAM, which only its guest uses.
    unsafe { host(0).write_bytes(0x11, crate::sys::page_size()) };
    map_file_over(host(1), 0x66);
    rebind(host(1), page(1));
    // The vCPU's thread, which destroys it as it ends, before the VM goes.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut vcpus = vcpus;
            let vcpu = &mut vcpus[0];
            let boot = BootCpu { pc: RAM_ADDR, x: [REPORT, page(0), page(1), 0] };
            vcpu.set_boot_cpu(&boot).unwrap();
            assert_eq!(report(vcpu), byte(0x11));
            map_file_over(host(0), 0x33);
            assert_eq!(report(vcpu), byte(0x11), "the guest follows no remap of the host's");
            rebind(host(0), page(0));
            assert_eq!(report(vcpu), byte(0x33), "mapped anew, the guest reads the file");
            assert_eq!(mincore(host(0)) & MINCORE_ANONYMOUS, 0, "the file's page, read");
            // SAFETY: As above.
            unsafe { host(0).cast::<u64>().write(0x88) };
            assert_ne!(mincore(host(0)) & MINCORE_ANONYMOUS, 0, "copied as the host wrote it");
            assert_eq!(report(vcpu), 0x88, "the guest reads the host's copy");

            assert_eq!(report(vcpu), byte(0x66));
            report(vcpu);
            // SAFETY: As above.
            assert_eq!(unsafe { host(1).add(8).cast::<u64>().read() }, 0x55);
            assert_ne!(mincore(host(1)) & MINCORE_ANONYMOUS, 0, "copied as the guest wrote it");
            // SAFETY: As above.
            unsafe { host(1).cast::<u64>().write(0x77) };
            assert_eq!(report(vcpu), 0x77);
        });
    });
}
