// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest frames into the network stack: inspection, smoltcp, the DNS
//! server and the relay, with a fake host (libmachine::net::fuzz).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    libmachine::net::fuzz::guest_frames(data);
});
