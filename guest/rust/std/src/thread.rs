// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Threads, which the kernel's Rust does not start: futures run on the kernel's workers instead.
//! Parking returns at once, as a spurious wakeup may, letting other tasks run: unparking does
//! nothing.

use crate::io;
use crate::time::{Duration, Instant};
use alloc_crate::boxed::Box;
use alloc_crate::string::String;

pub use crate::thread_local;
pub use local::{AccessError, LocalKey};

/// A thread's result: what it returned, or what it panicked with.
pub type Result<T> = core::result::Result<T, Box<dyn core::any::Any + Send + 'static>>;

/// Never: panics abort.
pub fn panicking() -> bool {
    false
}

/// Sleeps for `dur`, on a futex nothing wakes.
pub fn sleep(dur: Duration) {
    use crate::sys::sync::futex::{Futex, futex_wait};
    let futex = Futex::new(0);
    let start = Instant::now();
    while let Some(left) = dur.checked_sub(start.elapsed()) {
        futex_wait(&futex, 0, Some(left));
    }
}

/// Lets other tasks run, if the scheduler wants it to.
pub fn yield_now() {
    crate::sys::relax();
}

/// Returns, as a spurious wakeup may.
pub fn park() {
    crate::sys::relax();
}

/// Returns, as a spurious wakeup may.
pub fn park_timeout(_dur: Duration) {
    crate::sys::relax();
}

/// A thread's identity: its kernel task's, while it lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(core::num::NonZeroU64);

/// The current thread, which can be unparked, to no effect.
#[derive(Clone, Debug)]
pub struct Thread(ThreadId);

impl Thread {
    pub fn unpark(&self) {}

    pub fn name(&self) -> Option<&str> {
        None
    }

    pub fn id(&self) -> ThreadId {
        self.0
    }
}

/// The current thread. Interrupts, which have no thread of their own, share one identity.
pub fn current() -> Thread {
    // SAFETY: Returns the current task's pointer as an opaque identifier, or 0 in an interrupt.
    let task = unsafe { crate::sys::cladefs_rt_current_task() };
    Thread(ThreadId(core::num::NonZeroU64::new(task).unwrap_or(core::num::NonZeroU64::MIN)))
}

/// A thread's handle, which is never had.
pub struct JoinHandle<T>(core::marker::PhantomData<T>);

impl<T> JoinHandle<T> {
    pub fn join(self) -> Result<T> {
        unreachable!("no thread was spawned")
    }

    pub fn thread(&self) -> &Thread {
        unreachable!("no thread was spawned")
    }
}

/// Panics: the kernel's Rust starts no threads.
pub fn spawn<F: FnOnce() -> T + Send + 'static, T: Send + 'static>(_f: F) -> JoinHandle<T> {
    panic!("the kernel's Rust starts no threads")
}

/// Builds threads, which fails.
pub struct Builder(());

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self(())
    }

    pub fn name(self, _name: String) -> Self {
        self
    }

    pub fn stack_size(self, _size: usize) -> Self {
        self
    }

    pub fn spawn<F: FnOnce() -> T + Send + 'static, T: Send + 'static>(
        self,
        _f: F,
    ) -> io::Result<JoinHandle<T>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

mod local {
    //! Thread-local values are forbidden in the guest kernel (board 6929): `thread_local!` still
    //! compiles in unused library code, but any reachable `LocalKey::with` call references the
    //! undefined symbol `cladefs_error_thread_local_used_in_guest` so the guest kernel link fails
    //! loudly rather than needing a `task_struct` patch.

    use core::fmt;
    use core::marker::PhantomData;

    unsafe extern "C" {
        fn cladefs_error_thread_local_used_in_guest() -> !;
    }

    /// A value `thread_local!` declares.
    pub struct LocalKey<T: 'static> {
        _marker: PhantomData<fn() -> T>,
    }

    /// An error accessing a value.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AccessError;

    impl fmt::Display for AccessError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("already destroyed")
        }
    }

    impl<T: 'static> LocalKey<T> {
        #[doc(hidden)]
        pub const fn new(_init: fn() -> T) -> Self {
            Self { _marker: PhantomData }
        }

        pub fn with<F: FnOnce(&T) -> R, R>(&'static self, _f: F) -> R {
            // SAFETY: Intentionally undefined symbol; any reachable call fails the kernel link.
            unsafe { cladefs_error_thread_local_used_in_guest() }
        }

        pub fn try_with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> Result<R, AccessError> {
            Ok(self.with(f))
        }
    }

    impl<T: 'static> LocalKey<core::cell::Cell<T>> {
        pub fn set(&'static self, value: T) {
            self.with(|cell| cell.set(value))
        }

        pub fn get(&'static self) -> T
        where
            T: Copy,
        {
            self.with(core::cell::Cell::get)
        }

        pub fn take(&'static self) -> T
        where
            T: Default,
        {
            self.with(core::cell::Cell::take)
        }

        pub fn replace(&'static self, value: T) -> T {
            self.with(|cell| cell.replace(value))
        }
    }

    impl<T: 'static> LocalKey<core::cell::RefCell<T>> {
        pub fn with_borrow<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> R {
            self.with(|cell| f(&cell.borrow()))
        }

        pub fn with_borrow_mut<F: FnOnce(&mut T) -> R, R>(&'static self, f: F) -> R {
            self.with(|cell| f(&mut cell.borrow_mut()))
        }

        pub fn set(&'static self, value: T) {
            self.with(|cell| *cell.borrow_mut() = value)
        }

        pub fn take(&'static self) -> T
        where
            T: Default,
        {
            self.with(|cell| cell.take())
        }

        pub fn replace(&'static self, value: T) -> T {
            self.with(|cell| cell.replace(value))
        }
    }
}

/// Declares values each thread has its own of (see `LocalKey`).
#[macro_export]
macro_rules! thread_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = const { $init:expr }; $($rest:tt)*) => {
        $crate::thread_local!($(#[$attr])* $vis static $name: $t = $init; $($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = const $init:block; $($rest:tt)*) => {
        $crate::thread_local!($(#[$attr])* $vis static $name: $t = $init; $($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr; $($rest:tt)*) => {
        $(#[$attr])* $vis static $name: $crate::thread::LocalKey<$t> = {
            fn init() -> $t {
                $init
            }
            $crate::thread::LocalKey::new(init)
        };
        $crate::thread_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr) => {
        $crate::thread_local!($(#[$attr])* $vis static $name: $t = $init;);
    };
}
