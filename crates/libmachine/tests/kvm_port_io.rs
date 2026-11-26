// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Port I/O reaches devices whole: string instructions are several accesses,
//! wide accesses span consecutive ports (board 13171); and a machine keeps only
//! a bounded tail of its serial output (board 13163).
//!
//! On this host KVM delivers `rep insb` several bytes per exit (the old
//! handling read only the first: rep_insb_reads_every_byte failed) but
//! `rep outsb` one byte per exit, so rep_outsb_writes_every_byte passes with
//! or without the fix; the KVM API allows several per exit either way, and
//! the bus handles both. The word access failed with the old handling too.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{asm16, kvm_available, load};
use iced_x86::code_asm::*;
use libmachine::{ExitReason, Machine, MachineConfig};

const BUF: u32 = 0x2000;
const OUT: u32 = 0x500;

fn machine(config: MachineConfig, code: &[u8]) -> Machine {
    let mut m = Machine::new(config).expect("create machine");
    load(&mut m, code);
    m
}

#[test]
fn rep_outsb_writes_every_byte() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(si, BUF)?;
        a.mov(cx, 11)?;
        a.mov(dx, 0x3f8)?;
        a.rep().outsb()?;
        a.hlt()
    });
    let mut m = machine(MachineConfig::default(), &code);
    m.load(BUF.into(), b"hello world").unwrap();
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.serial_output(), b"hello world");
}

#[test]
fn rep_insb_reads_every_byte() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(di, OUT)?;
        a.mov(cx, 4)?;
        a.mov(dx, 0x3fd)?; // line status: transmitter empty and idle
        a.rep().insb()?;
        a.hlt()
    });
    let mut m = machine(MachineConfig::default(), &code);
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.read_memory(OUT.into(), 4).unwrap(), [0x60; 4]);
}

#[test]
fn a_word_access_spans_two_byte_ports() {
    if !kvm_available() {
        return;
    }
    // outw to 0x3f8 writes 0x41 to THR (0x3f8) and 0x01 to IER (0x3f9);
    // inw from 0x3f8 reads RBR (empty: 0) and IER back.
    let code = asm16(|a| {
        a.mov(dx, 0x3f8)?;
        a.mov(ax, 0x0141)?;
        a.out(dx, ax)?;
        a.in_(ax, dx)?;
        a.hlt()
    });
    let mut m = machine(MachineConfig::default(), &code);
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.serial_output(), b"A");
    assert_eq!(m.registers().unwrap().rax & 0xffff, 0x0100, "IER read back in the high byte");
}

/// Writes `chunks` x 4096 bytes through COM1 with `rep outsb`.
fn flood(chunks: u16) -> Vec<u8> {
    asm16(|a| {
        let mut outer = a.create_label();
        a.mov(bx, chunks as u32)?;
        a.mov(dx, 0x3f8)?;
        a.set_label(&mut outer)?;
        a.mov(si, BUF)?;
        a.mov(cx, 4096)?;
        a.rep().outsb()?;
        a.dec(bx)?;
        a.jnz(outer)?;
        a.hlt()
    })
}

#[derive(Clone, Default)]
struct Counter(Arc<AtomicU64>);
impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn serial_output_keeps_a_bounded_tail_and_the_console_sees_everything() {
    if !kvm_available() {
        return;
    }
    // KVM exits once per byte of `rep outsb` here (~8 us each), so this is
    // kept small; that the buffer never exceeds 1.5 x the limit is checked by
    // serial.rs's unit test.
    const LIMIT: usize = 4 << 10;
    const CHUNKS: u16 = 16; // 64 KiB
    let config = MachineConfig { serial_capture_limit: LIMIT, ..Default::default() };
    let mut m = machine(config, &flood(CHUNKS));
    let pattern: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    m.load(BUF.into(), &pattern).unwrap();
    let console = Counter::default();
    m.set_console(console.clone());

    assert_eq!(m.run().unwrap(), ExitReason::Halt);

    assert_eq!(console.0.load(Ordering::Relaxed), u64::from(CHUNKS) * 4096);
    let kept = m.serial_output();
    assert_eq!(kept.len(), LIMIT);
    assert_eq!(&kept[kept.len() - 4096..], &pattern[..], "the tail is the most recent output");
    assert_eq!(m.take_serial_output().len(), LIMIT);
    assert!(m.serial_output().is_empty());
}
