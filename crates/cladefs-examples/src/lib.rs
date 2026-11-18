// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: cladefs's examples (`examples/`), in a package whose dependencies turn on none of
//! its tests' features, so that a release build of one, as its measurements are, has none of them
//! (TASK-320). Nothing here but them.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]
