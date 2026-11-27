# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs small real-mode guests, written in assembly, through the Python bindings."""

import os
import threading

import pytest

import libmachine
from guest import ENTRY, asm, machine_with, serial_print, store_cpuid
from libmachine import Machine, MachineError

requires_kvm = pytest.mark.skipif(
    not os.access("/dev/kvm", os.R_OK | os.W_OK),
    reason="/dev/kvm is not accessible",
)


@requires_kvm
def test_serial_output_and_halt():
    m = machine_with(serial_print("hello from the guest\n"))
    assert m.run().kind == "halt"
    assert m.serial_output() == b"hello from the guest\n"
    assert m.take_serial_output() == b"hello from the guest\n"
    assert m.serial_output() == b""


@requires_kvm
def test_exit_counts():
    m = machine_with(serial_print("four"))
    assert m.exit_counts() == dict.fromkeys(
        ["serial", "rtc", "acpi", "unclaimed_io", "pci_config", "pci_bar", "unclaimed_mmio",
         "halt", "interrupted", "startup", "hypercall", "wait",
         "system_register", "access_retried", "other"],
        0,
    )
    assert m.run().kind == "halt"
    counts = m.exit_counts()
    assert (counts["serial"], counts["halt"]) == (4, 1)
    assert sum(counts.values()) == 5


@requires_kvm
def test_registers():
    program = asm("""
        mov ax, 0x1234
        mov bx, 0x5678
        add ax, bx
        hlt
    """)
    m = Machine()
    m.load(ENTRY, program)
    m.set_entry(ENTRY)
    assert m.run().kind == "halt"
    regs = m.registers()
    assert regs.rax & 0xFFFF == 0x68AC
    assert regs.rip == ENTRY + len(program), "rip points past hlt"
    assert "rax=0x68ac" in repr(regs)


@requires_kvm
def test_guest_polls_serial_line_status():
    m = machine_with("""
        mov dx, 0x3fd       ; COM1 line status register
        in al, dx
        hlt
    """)
    assert m.run().kind == "halt"
    assert m.registers().rax & 0xFF == 0x60, "transmitter empty"


@requires_kvm
def test_unhandled_io_exit_fields():
    m = machine_with("""
        mov al, 1
        out 0x80, al
        hlt
    """)
    exit = m.run()
    assert (exit.kind, exit.port, exit.size, exit.write) == ("unhandled_io", 0x80, 1, True)
    assert exit.addr is None
    assert m.run().kind == "halt", "the guest resumes after an unhandled access"


@requires_kvm
def test_unhandled_mmio_exit_fields():
    m = machine_with(
        """
        mov ax, 0xf000
        mov ds, ax
        mov byte [0], 1     ; physical 0xf0000, past the end of RAM
        hlt
        """,
        mem_size=0x10000,
    )
    exit = m.run()
    assert (exit.kind, exit.addr, exit.write) == ("unhandled_mmio", 0xF0000, True)


@requires_kvm
def test_memory_round_trip():
    m = Machine()
    m.load(0x2000, b"hello")
    assert m.read_memory(0x2000, 5) == b"hello"
    assert m.mem_size == 16 << 20


@requires_kvm
def test_machines_run_in_parallel_threads():
    results = {}

    def worker(i: int) -> None:
        m = machine_with(serial_print(str(i)))
        assert m.run().kind == "halt"
        results[i] = m.serial_output()

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert results == {i: str(i).encode() for i in range(8)}


def test_invalid_mem_size_raises_value_error():
    with pytest.raises(ValueError, match="multiple of 4096"):
        Machine(mem_size=1000)


@requires_kvm
def test_errors_are_python_exceptions():
    m = Machine()
    with pytest.raises(ValueError):
        m.set_entry(0x10000)
    with pytest.raises(ValueError):
        m.load(m.mem_size, b"x")
    with pytest.raises(ValueError):
        m.read_memory(m.mem_size - 1, 2)
    assert issubclass(MachineError, Exception)


def test_public_api():
    assert set(libmachine.__all__) == {
        "GATEWAY_ADDR",
        "GUEST_ADDR",
        "GuestBusyError",
        "Agent",
        "AsyncProcess",
        "AsyncSandbox",
        "ConnectionRecord",
        "ConnectionRequest",
        "EgressPolicy",
        "ExecResult",
        "Exit",
        "Machine",
        "MachineError",
        "Network",
        "NetworkConfig",
        "Process",
        "Registers",
        "SNAPSHOT_FORMAT_VERSION",
        "Snapshot",
        "StopHandle",
        "Vsock",
    }


def guest_cpuid(m: Machine, leaf: int, subleaf: int = 0) -> tuple[int, int, int, int]:
    """Runs CPUID in the guest and returns what it saw."""
    m.load(ENTRY, asm(store_cpuid(leaf, subleaf, out=0x500)))
    m.set_entry(ENTRY)
    assert m.run().kind == "halt"
    raw = m.read_memory(0x500, 16)
    eax, ebx, ecx, edx = (int.from_bytes(raw[i:i + 4], "little") for i in range(0, 16, 4))
    return eax, ebx, ecx, edx


@requires_kvm
@pytest.mark.parametrize("model", ["host", "baseline"])
def test_cpu_model(model):
    m = Machine(cpu_model=model)
    assert m.cpu_model == model
    assert len(m.cpu_fingerprint) == 64
    _, _, ecx, _ = guest_cpuid(m, 1)
    assert ecx & (1 << 31), "hypervisor bit"
    assert not ecx & (1 << 5), "no VMX"
    assert m.cpuid(1)[2] & (1 << 31), "configured table agrees"
    assert m.cpuid(0xDEAD_BEEF) is None


@requires_kvm
def test_baseline_hides_avx512():
    m = Machine(cpu_model="baseline")
    _, ebx, _, _ = guest_cpuid(m, 7, 0)
    assert not ebx & (1 << 16), "avx512f"
    assert ebx & (1 << 5), "avx2"


@requires_kvm
def test_same_model_same_fingerprint():
    assert Machine().cpu_fingerprint == Machine().cpu_fingerprint


def test_unknown_cpu_model_is_value_error():
    with pytest.raises(ValueError, match="unknown CPU model"):
        Machine(cpu_model="pentium")


def test_too_much_ram_is_value_error():
    # 1 TiB - 1 GiB is the most: RAM from 4 GiB would reach the 64-bit MMIO window.
    with pytest.raises(ValueError, match="at most"):
        Machine(mem_size=(1 << 40) - (1 << 30) + 4096)


def test_bare_machine_has_one_vcpu():
    assert Machine().vcpus == 1
    with pytest.raises(ValueError, match="exactly one vCPU"):
        Machine(vcpus=2)
