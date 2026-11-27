// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the rings synchronize with: std's atomics and fences; or, with the `loom` feature, loom's
//! (tokio-rs/loom), whose model then runs every interleaving of a model's threads under the
//! orderings each access asks for, which Shuttle, seeing every atomic as `SeqCst`, cannot
//! (docs/memory-ordering.md). Only for the models, as loom's atomics work only in one.

#[cfg(not(feature = "loom"))]
pub(crate) use std::sync::atomic;

#[cfg(feature = "loom")]
pub(crate) use loom::sync::atomic;
