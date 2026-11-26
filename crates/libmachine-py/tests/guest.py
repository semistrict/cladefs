# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Builds the tests' guest programs from x86 assembly with Keystone."""

import threading

from keystone import KS_ARCH_X86, KS_MODE_16, KS_OPT_SYNTAX_NASM, Ks

import libmachine

# Where test programs are loaded and start, in 16-bit real mode.
ENTRY = 0x1000

_ks = Ks(KS_ARCH_X86, KS_MODE_16)
_ks.syntax = KS_OPT_SYNTAX_NASM
# Keystone is not thread-safe, and ctypes releases the GIL while it runs.
_ks_lock = threading.Lock()


def asm(source: str, origin: int = ENTRY) -> bytes:
    """Assembles 16-bit real-mode code (NASM syntax) to run at `origin`."""
    with _ks_lock:
        encoding, _ = _ks.asm(source, origin)
    return bytes(encoding)


def machine_with(source: str, **config) -> libmachine.Machine:
    """A machine with `source` assembled, loaded and set to start at ENTRY."""
    m = libmachine.Machine(**config)
    m.load(ENTRY, asm(source))
    m.set_entry(ENTRY)
    return m


def serial_print(text: str) -> str:
    """A program that writes `text` to COM1 and halts."""
    escaped = text.encode().hex()
    data = ", ".join(f"0x{escaped[i:i + 2]}" for i in range(0, len(escaped), 2))
    return f"""
        mov si, msg
        mov dx, 0x3f8       ; COM1 transmit register
    next:
        lodsb
        test al, al
        jz done
        out dx, al
        jmp next
    done:
        hlt
    msg:
        db {data}, 0
    """


def store_cpuid(leaf: int, subleaf: int = 0, out: int = 0x500) -> str:
    """A program that runs CPUID and stores EAX, EBX, ECX, EDX at `out`."""
    return f"""
        mov eax, {leaf:#x}
        mov ecx, {subleaf:#x}
        cpuid
        mov [{out:#x}], eax
        mov [{out + 4:#x}], ebx
        mov [{out + 8:#x}], ecx
        mov [{out + 12:#x}], edx
        hlt
    """
