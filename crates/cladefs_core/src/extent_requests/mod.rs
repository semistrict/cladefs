// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! doc-4's requests, and their interpreter: what the extent device's guests and a cell's local
//! processes ask of a cell, served by one interpreter whatever carries them (board 14378). A
//! guest's come on its device's virtqueues, which its VMM serves (`virtio_extent`); a local
//! process's (the VMM's own client of the extent daemon among them) on a ring in memory it shares
//! with the daemon, which the daemon serves. Each transport is a [`Dialect`] of the protocol, a type
//! parameter of [`handle`]: the flags its requests take, and the types it serves alone.
//!
//! - [`protocol`]: the requests, flags, features and configuration space, and the version a
//!   guest's driver and a snapshot are checked against.
//! - [`Buffers`], [`Reader`] and [`Writer`]: a request's memory, as its descriptors name it.
//! - [`Backend`] and [`GuestBackend`]: the cell a request is served with.
//! - [`handle`], [`Dialect`] and [`Process`]: serving a request, as a dialect has it.
//! - [`Guest`] and [`GuestNotices`]: a guest's dialect, whoever serves its virtqueues.

mod backend;
mod buffers;
mod guest;
mod interpret;
pub mod protocol;

pub use backend::{Backend, GuestBackend};
pub use buffers::{Buffers, BuffersError, Reader, Writer};
pub use guest::{Guest, GuestNotices};
pub use interpret::{Dialect, Handled, Process, handle};
