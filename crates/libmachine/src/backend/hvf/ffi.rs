// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hypervisor.framework's C API, the part libmachine calls, declared by hand from the SDK's
//! headers (`Hypervisor.framework/Headers`, macOS 15 and later for the GIC): no bindgen, no
//! crate between. The types are the headers' widths; the enums their values.

#![allow(non_camel_case_types)]

use std::ffi::c_void;

pub(crate) type hv_return_t = i32;
pub(crate) type hv_simd_fp_reg_t = u32;
pub(crate) type hv_gic_icc_reg_t = u16;

/// `hv_vcpu_sme_state_t` (macOS 15.2): PSTATE.SM and PSTATE.ZA.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct hv_vcpu_sme_state_t {
    pub(crate) streaming_sve_mode_enabled: bool,
    pub(crate) za_storage_enabled: bool,
}
pub(crate) type hv_vcpu_t = u64;
pub(crate) type hv_ipa_t = u64;
pub(crate) type hv_memory_flags_t = u64;
pub(crate) type hv_reg_t = u32;
pub(crate) type hv_sys_reg_t = u16;
pub(crate) type hv_exit_reason_t = u32;
/// An OS object (`hv_vm_config_t`, `hv_gic_config_t`), released with `os_release`.
pub(crate) type os_object_t = *mut c_void;

pub(crate) const HV_SUCCESS: hv_return_t = 0;

pub(crate) const HV_MEMORY_READ: hv_memory_flags_t = 1 << 0;
pub(crate) const HV_MEMORY_WRITE: hv_memory_flags_t = 1 << 1;
pub(crate) const HV_MEMORY_EXEC: hv_memory_flags_t = 1 << 2;

pub(crate) const HV_EXIT_REASON_CANCELED: hv_exit_reason_t = 0;
pub(crate) const HV_EXIT_REASON_EXCEPTION: hv_exit_reason_t = 1;
pub(crate) const HV_EXIT_REASON_VTIMER_ACTIVATED: hv_exit_reason_t = 2;

/// `HV_REG_X0`; X1 to X30 follow it.
pub(crate) const HV_REG_X0: hv_reg_t = 0;
pub(crate) const HV_REG_PC: hv_reg_t = 31;
pub(crate) const HV_REG_CPSR: hv_reg_t = 34;

pub(crate) const HV_SYS_REG_MPIDR_EL1: hv_sys_reg_t = 0xc005;
pub(crate) const HV_SYS_REG_SCTLR_EL1: hv_sys_reg_t = 0xc080;
pub(crate) const HV_SYS_REG_ID_AA64PFR1_EL1: hv_sys_reg_t = 0xc021;
/// EL2's, which a VM with EL2 has (macOS 15).
pub(crate) const HV_SYS_REG_SCTLR_EL2: hv_sys_reg_t = 0xe080;
pub(crate) const HV_SYS_REG_CNTV_CTL_EL0: hv_sys_reg_t = 0xdf19;
pub(crate) const HV_SYS_REG_CNTV_CVAL_EL0: hv_sys_reg_t = 0xdf1a;

/// `hv_ipa_granule_t` (macOS 26).
pub(crate) const HV_IPA_GRANULE_4KB: u32 = 0;

/// `hv_vm_config_set_ipa_granule`.
pub(crate) type hv_vm_config_set_ipa_granule_t =
    unsafe extern "C" fn(config: os_object_t, granule: u32) -> hv_return_t;

/// `hv_vm_config_set_ipa_granule`, macOS 26's, if this host's framework has it: looked up as the
/// process runs, not linked, since an SDK before macOS 26's has no stub for it to link against,
/// and a host before it no function (board 13741).
pub(crate) fn hv_vm_config_set_ipa_granule() -> Option<hv_vm_config_set_ipa_granule_t> {
    // SAFETY: A NUL-terminated name, looked up in every image loaded, Hypervisor.framework's
    // among them (linked below).
    let symbol =
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"hv_vm_config_set_ipa_granule".as_ptr()) };
    // SAFETY: The framework's function of this name has the type its header gives it.
    (!symbol.is_null()).then(|| unsafe {
        std::mem::transmute::<*mut c_void, hv_vm_config_set_ipa_granule_t>(symbol)
    })
}

/// `hv_vcpu_exit_exception_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct hv_vcpu_exit_exception_t {
    /// ESR_EL2's syndrome.
    pub(crate) syndrome: u64,
    /// FAR_EL2: the guest's virtual address.
    pub(crate) virtual_address: u64,
    /// The guest physical address, for a stage 2 fault.
    pub(crate) physical_address: hv_ipa_t,
}

/// `hv_vcpu_exit_t`, which `hv_vcpu_create` points the caller at for the vCPU's life.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct hv_vcpu_exit_t {
    pub(crate) reason: hv_exit_reason_t,
    pub(crate) exception: hv_vcpu_exit_exception_t,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn hv_vm_config_create() -> os_object_t;
    pub(crate) fn hv_vm_config_get_max_ipa_size(ipa_bit_length: *mut u32) -> hv_return_t;
    pub(crate) fn hv_vm_config_set_ipa_size(
        config: os_object_t,
        ipa_bit_length: u32,
    ) -> hv_return_t;
    /// Nested virtualization (macOS 15): whether this host gives VMs EL2.
    pub(crate) fn hv_vm_config_get_el2_supported(el2_supported: *mut bool) -> hv_return_t;
    pub(crate) fn hv_vm_config_set_el2_enabled(
        config: os_object_t,
        el2_enabled: bool,
    ) -> hv_return_t;
    pub(crate) fn hv_vm_create(config: os_object_t) -> hv_return_t;
    pub(crate) fn hv_vm_destroy() -> hv_return_t;
    pub(crate) fn hv_vm_get_max_vcpu_count(max_vcpu_count: *mut u32) -> hv_return_t;
    pub(crate) fn hv_vm_map(
        addr: *mut c_void,
        ipa: hv_ipa_t,
        size: usize,
        flags: hv_memory_flags_t,
    ) -> hv_return_t;
    pub(crate) fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;

    pub(crate) fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *const hv_vcpu_exit_t,
        config: os_object_t,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_sys_reg(
        vcpu: hv_vcpu_t,
        reg: hv_sys_reg_t,
        value: *mut u64,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_sys_reg(
        vcpu: hv_vcpu_t,
        reg: hv_sys_reg_t,
        value: u64,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    pub(crate) fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, vcpu_count: u32) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_vtimer_offset(
        vcpu: hv_vcpu_t,
        vtimer_offset: *mut u64,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, vtimer_offset: u64) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_vtimer_mask(vcpu: hv_vcpu_t, masked: *mut bool) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, masked: bool) -> hv_return_t;
    /// `hv_simd_fp_uchar16_t` is a 16-byte vector, written here as its bytes.
    pub(crate) fn hv_vcpu_get_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: hv_simd_fp_reg_t,
        value: *mut [u8; 16],
    ) -> hv_return_t;
    /// Takes its value, a 16-byte vector, in a SIMD register (v0), which no Rust signature can
    /// say: called through [`hv_vcpu_set_simd_fp_reg`] alone.
    #[link_name = "hv_vcpu_set_simd_fp_reg"]
    fn hv_vcpu_set_simd_fp_reg_symbol();

    // SME (macOS 15.2).
    pub(crate) fn hv_sme_config_get_max_svl_bytes(value: *mut usize) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_sme_state(
        vcpu: hv_vcpu_t,
        sme_state: *mut hv_vcpu_sme_state_t,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_sme_state(
        vcpu: hv_vcpu_t,
        sme_state: *const hv_vcpu_sme_state_t,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_sme_z_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: *mut u8,
        length: usize,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_sme_z_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: *const u8,
        length: usize,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_sme_p_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: *mut u8,
        length: usize,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_sme_p_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: *const u8,
        length: usize,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_sme_za_reg(
        vcpu: hv_vcpu_t,
        value: *mut u8,
        length: usize,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_sme_za_reg(
        vcpu: hv_vcpu_t,
        value: *const u8,
        length: usize,
    ) -> hv_return_t;
    pub(crate) fn hv_vcpu_get_sme_zt0_reg(vcpu: hv_vcpu_t, value: *mut [u8; 64]) -> hv_return_t;
    pub(crate) fn hv_vcpu_set_sme_zt0_reg(vcpu: hv_vcpu_t, value: *const [u8; 64]) -> hv_return_t;

    pub(crate) fn hv_gic_config_create() -> os_object_t;
    pub(crate) fn hv_gic_config_set_distributor_base(
        config: os_object_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub(crate) fn hv_gic_config_set_redistributor_base(
        config: os_object_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub(crate) fn hv_gic_config_set_msi_region_base(
        config: os_object_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub(crate) fn hv_gic_config_set_msi_interrupt_range(
        config: os_object_t,
        msi_intid_base: u32,
        msi_intid_count: u32,
    ) -> hv_return_t;
    pub(crate) fn hv_gic_create(config: os_object_t) -> hv_return_t;
    pub(crate) fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_get_redistributor_size(size: *mut usize) -> hv_return_t;
    /// The guest physical address of `vcpu`'s redistributor.
    pub(crate) fn hv_gic_get_redistributor_base(
        vcpu: hv_vcpu_t,
        base: *mut hv_ipa_t,
    ) -> hv_return_t;
    pub(crate) fn hv_gic_get_redistributor_region_size(size: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_get_distributor_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_get_redistributor_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_get_msi_region_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_get_msi_region_size(size: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t;
    pub(crate) fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
    pub(crate) fn hv_gic_send_msi(address: hv_ipa_t, intid: u32) -> hv_return_t;
    /// A vCPU's GIC CPU interface register, from its own thread.
    pub(crate) fn hv_gic_get_icc_reg(
        vcpu: hv_vcpu_t,
        reg: hv_gic_icc_reg_t,
        value: *mut u64,
    ) -> hv_return_t;
    pub(crate) fn hv_gic_set_icc_reg(
        vcpu: hv_vcpu_t,
        reg: hv_gic_icc_reg_t,
        value: u64,
    ) -> hv_return_t;
    /// The GIC's state but its CPU interfaces', with the VM stopped: an OS object, released with
    /// `os_release`; null if the framework can't represent it.
    pub(crate) fn hv_gic_state_create() -> os_object_t;
    pub(crate) fn hv_gic_state_get_size(state: os_object_t, size: *mut usize) -> hv_return_t;
    pub(crate) fn hv_gic_state_get_data(state: os_object_t, data: *mut c_void) -> hv_return_t;
    /// Puts back what `hv_gic_state_get_data` gave: once every vCPU exists, before any runs.
    pub(crate) fn hv_gic_set_state(data: *const c_void, size: usize) -> hv_return_t;
}

/// `hv_vcpu_set_simd_fp_reg(vcpu, reg, value)`: its value, a 16-byte vector, passed in v0 as
/// AAPCS64 passes a short vector, the call made in assembly since Rust's FFI takes no SIMD
/// types by value.
///
/// # Safety
///
/// As the framework's: called on the thread that owns `vcpu`.
pub(crate) unsafe fn hv_vcpu_set_simd_fp_reg(
    vcpu: hv_vcpu_t,
    reg: hv_simd_fp_reg_t,
    value: [u8; 16],
) -> hv_return_t {
    // SAFETY: 16 bytes are a 16-byte vector.
    let value: std::arch::aarch64::uint8x16_t = unsafe { std::mem::transmute(value) };
    let ret: u64;
    // SAFETY: a call of the framework's function by the C ABI: its arguments in x0, w1 and v0,
    // its result in w0, every register a call may change marked clobbered.
    unsafe {
        std::arch::asm!(
            "bl {f}",
            f = sym hv_vcpu_set_simd_fp_reg_symbol,
            inlateout("x0") vcpu => ret,
            in("x1") u64::from(reg),
            in("v0") value,
            clobber_abi("C"),
        );
    }
    ret as u32 as hv_return_t
}

/// `mach_timebase_info_data_t`: host ticks to nanoseconds, `numer / denom`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct mach_timebase_info_t {
    pub(crate) numer: u32,
    pub(crate) denom: u32,
}

unsafe extern "C" {
    /// The ratio of the host's tick to a nanosecond.
    pub(crate) fn mach_timebase_info(info: *mut mach_timebase_info_t) -> i32;
    /// libSystem's release of an OS object.
    pub(crate) fn os_release(object: os_object_t);
    /// The host's monotonic tick count, which the guest's virtual counter is offset from.
    pub(crate) fn mach_absolute_time() -> u64;
}
