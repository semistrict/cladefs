// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A runtime for Rust in the Linux kernel, without std, over the C functions of
//! guest/linux/fs/cladefs/rt.h:
//!
//! - the global allocator, over kvmalloc;
//! - a logger for the `log` crate, over printk;
//! - the panic handler, which panics the kernel;
//! - an executor: `block_on` sleeps the calling thread until a future is done, `spawn` runs
//!   futures on kernel workers, and `Timer` wakes them after a while;
//! - randomness, from the kernel's generator.
//!
//! The kernel's threads have small stacks (stock 16 KiB, TASK-26.9), so large futures are polled
//! from the heap while small ones stay inline on the stack (board 6014).

#![no_std]

extern crate alloc;

mod allocator;
mod executor;
mod ffi;
mod logger;
mod panic;
mod random;
mod time;

pub use executor::{Task, block_on, spawn, spawn_urgent};
pub use logger::init_logger;
pub use random::fill_random;
pub use time::{Timer, monotonic};
