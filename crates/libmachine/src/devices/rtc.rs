// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The arm64 board's real-time clock: an ARM PL031, whose count of seconds is
//! the host's wall clock (plus whatever offset the guest loaded), so that a
//! guest's kernel sets its clock as it boots (RTC_HCTOSYS) rather than
//! starting at 1970, which nothing on a network survives (TLS, signed package
//! indexes). Its alarm is never raised: guests use their timers for that.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The data register: the current count.
const RTCDR: u64 = 0x000;
/// The match register: the alarm's count.
const RTCMR: u64 = 0x004;
/// The load register: a count to go on from.
const RTCLR: u64 = 0x008;
/// The control register: bit 0 starts the clock, which always runs here.
const RTCCR: u64 = 0x00c;
/// The PrimeCell peripheral and cell IDs, a byte a word, from 0xfe0: part
/// 0x031, designer ARM (0x41), revision 0 (0x00041031, as Linux's AMBA bus
/// matches it); and the PrimeCell signature.
const ID: u64 = 0xfe0;
const IDS: [u8; 8] = [0x31, 0x10, 0x04, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

/// A PL031 counting the host's wall clock.
#[derive(Default)]
pub(crate) struct Rtc {
    /// What a load set the count off the host's clock by, in seconds.
    offset: AtomicI64,
    /// The alarm's count, as written (kept for reading back).
    alarm: std::sync::atomic::AtomicU32,
}

impl Rtc {
    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
    }

    fn count(&self) -> u32 {
        (Self::now() + self.offset.load(Ordering::Relaxed)) as u32
    }

    /// The 32-bit register at `offset` in the device's page.
    pub(crate) fn read(&self, offset: u64) -> u32 {
        match offset {
            RTCDR => self.count(),
            RTCMR => self.alarm.load(Ordering::Relaxed),
            RTCLR => self.count(),
            RTCCR => 1,
            o if (ID..ID + 4 * IDS.len() as u64).contains(&o) && o % 4 == 0 => {
                u32::from(IDS[((o - ID) / 4) as usize])
            }
            _ => 0,
        }
    }

    /// The clock's state for a snapshot: what the guest loaded, as the offset off the host's
    /// clock it set, and the alarm's count. A restore's clock goes on from the host's, as the
    /// snapshot's did.
    pub(crate) fn save(&self, e: &mut crate::snapshot::codec::Encoder) {
        e.i64(self.offset.load(Ordering::Relaxed));
        e.u32(self.alarm.load(Ordering::Relaxed));
    }

    /// Puts back what [`save`](Self::save) wrote.
    pub(crate) fn restore(
        &self,
        d: &mut crate::snapshot::codec::Decoder<'_>,
    ) -> crate::error::Result<()> {
        self.offset.store(d.i64()?, Ordering::Relaxed);
        self.alarm.store(d.u32()?, Ordering::Relaxed);
        Ok(())
    }

    /// Writes the 32-bit register at `offset`: a load, or the alarm's
    /// count; the rest (the interrupt's mask and clear) mean nothing here.
    pub(crate) fn write(&self, offset: u64, value: u32) {
        match offset {
            RTCLR => self.offset.store(i64::from(value) - Self::now(), Ordering::Relaxed),
            RTCMR => self.alarm.store(value, Ordering::Relaxed),
            _ => {}
        }
    }
}
