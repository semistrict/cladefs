// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[macro_export]
macro_rules! debug_assert_not_too_long {
    ($future:expr $(, $arg:tt)*) => {
        {
            #[cfg(debug_assertions)]
            {
                use futures::future::FutureExt;
                let future = $future.fuse();
                futures::pin_mut!(future);
                loop {
                    // cladefs: `select_biased!` avoids `futures::select!`'s thread-local PRNG,
                    // keeping the guest kernel free of `thread_local!` even with debug assertions
                    // enabled (board 6929).
                    futures::select_biased! {
                        result = future => break result,
                        () = fuchsia_async::Timer::new(std::time::Duration::from_secs(60)).fuse() =>
                            {
                                #[cfg(all(target_os = "fuchsia", not(doc)))]
                                ::debug::backtrace_request_all_threads();
                                #[cfg(not(target_os = "fuchsia"))]
                                panic!($($arg),*);
                            }
                    }
                }
            }
            #[cfg(not(debug_assertions))]
            $future.await
        }
    };
}
