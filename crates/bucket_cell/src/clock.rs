// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a bucket's clock, as a host reads it ([`crate::bucket::Bucket::now`], board 8467): the
//! backend's, which it dates objects by ([`crate::bucket::Meta::modified`]), never the host's own.
//! Every window measured from when an object was written (a mirror's pin going stale, read
//! evidence's [`crate::evidence::KEPT_FOR`]) and every time a collector lease holds
//! ([`crate::passed::CollectorLease`], board 8471) is measured by it: a host whose own clock is off
//! the backend's, or steps, measures each as every other host does, and as the backend dates.
//!
//! A backend over HTTP (S3, GCS, Azure) dates each of its responses (`Date`, RFC 9110 §6.6.1), in
//! whole seconds, by the clock it dates objects by. [`Served`] keeps the best lower bound they
//! give: the backend's time as a response arrives is at least its date, and since then it has gone
//! on at least as fast as the host's monotonic clock, less [`DRIFT_PPM`]. So the clock is never
//! ahead of the backend's: a window measured by it closes late, by a second and a round trip at
//! most of a busy bucket, never early; a lease it holds ends early by as much, and others find it
//! held as long. It assumes the backend's clock goes on, never back, as a cloud's does. A backend
//! of files or memory dates objects by the host's own clock, its clock ([`Clock::Host`]).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Formats `time` as UTC RFC 3339 (`YYYY-MM-DDTHH:MM:SS.<frac>Z`) with millisecond (`millis = true`)
/// or microsecond (`millis = false`) precision.
pub fn write_rfc3339_utc(
    f: &mut std::fmt::Formatter<'_>,
    time: SystemTime,
    millis: bool,
) -> std::fmt::Result {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let hour = sod / 3_600;
    let min = (sod % 3_600) / 60;
    let sec = sod % 60;
    // Howard Hinnant's civil_from_days: proleptic Gregorian date from days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);
    if millis {
        let ms = since.subsec_millis();
        write!(f, "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{ms:03}Z")
    } else {
        let us = since.subsec_micros();
        write!(f, "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{us:06}Z")
    }
}

/// Formats `time` as UTC RFC 3339 with millisecond precision (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub fn rfc3339_millis(time: SystemTime) -> String {
    struct Fmt(SystemTime);
    impl std::fmt::Display for Fmt {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write_rfc3339_utc(f, self.0, true)
        }
    }
    Fmt(time).to_string()
}

/// The most a host's monotonic clock may run fast of a backend's, in millionths: what NTP slews a
/// clock by at most, ten times a crystal's usual error. A clock that runs faster reads ahead of the
/// backend's by that much of the time since a response last dated it.
pub const DRIFT_PPM: u64 = 500;

/// The most a bucket's clock may be off the date its backend gives an object as it is written,
/// which a probe of the bucket checks ([`checked`]): a sixth of the shortest window measured
/// against such dates, the collector's grace period by default (60 s, `cladefs-gc --grace`). A
/// clock its backend's responses date reads within a second and a round trip of them.
pub const SKEW: u64 = 10_000;

/// Whether a bucket's clock, reading `now`, is its backend's, as a probe checks it: within
/// [`SKEW`] of `modified`, the date the backend gave an object written `since` ago, at most, or
/// later by no more than `since`. Off, what it is off by.
pub fn checked(modified: u64, now: u64, since: std::time::Duration) -> Result<(), String> {
    let since = u64::try_from(since.as_millis()).unwrap_or(u64::MAX);
    // 0, behind: a served clock no response dated ([`Served::now`]). A backend's own clock may
    // read 0 and date objects so, as the simulation's begins.
    if now == 0 && modified > SKEW {
        return Err("the bucket's clock has no time: none of its backend's responses carried a \
                    date it could read (`Date`)"
            .to_owned());
    }
    if now.saturating_add(SKEW) < modified {
        let behind = modified - now;
        return Err(format!(
            "the bucket's clock reads {now}, {behind} ms behind the date its backend gave it, \
             {modified}: more than {SKEW} ms off"
        ));
    }
    if now > modified.saturating_add(since).saturating_add(SKEW) {
        let ahead = now - modified;
        return Err(format!(
            "the bucket's clock reads {now}, {ahead} ms ahead of the date its backend gave it \
             {since} ms before, {modified}: more than {SKEW} ms off"
        ));
    }
    Ok(())
}

/// The clock a bucket measures time by: in milliseconds since the Unix epoch, as the backend
/// dates objects.
#[derive(Clone, Debug)]
pub enum Clock {
    /// The host's own: a backend of files or memory, which dates objects by it.
    Host,
    /// A backend's over HTTP, as its responses date them.
    Served(Arc<Served>),
}

impl Clock {
    /// The backend's time now, at least: never ahead of it.
    pub fn now(&self) -> u64 {
        match self {
            Clock::Host => {
                // Before the epoch, a host's clock is broken past measuring anything by.
                let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            }
            Clock::Served(served) => served.now(),
        }
    }
}

/// A backend's clock as its responses date them: the best lower bound they gave of it.
#[derive(Debug, Default)]
pub struct Served {
    best: Mutex<Option<Dated>>,
}

/// A response's date, `ms`, and when it arrived, `at`, by the host's monotonic clock.
#[derive(Clone, Copy, Debug)]
struct Dated {
    at: Instant,
    ms: u64,
}

impl Dated {
    /// The backend's time at `then`, at least, as this date says: its date, and the time since
    /// it arrived less [`DRIFT_PPM`] of it. Before it arrived, its date.
    fn at(self, then: Instant) -> u64 {
        let since = then.saturating_duration_since(self.at).as_millis();
        let since = u64::try_from(since).unwrap_or(u64::MAX);
        self.ms.saturating_add(since - since.saturating_mul(DRIFT_PPM) / 1_000_000)
    }
}

impl Served {
    /// The backend's time now, at least, as its responses dated it: 0 before any did, a lower
    /// bound still, which closes no window. A probe of the bucket fails on it
    /// ([`crate::object_bucket::probe`]).
    pub fn now(&self) -> u64 {
        self.best().map_or(0, |best| best.at(Instant::now()))
    }

    /// A response dated `date`, an HTTP date, arrived at `at`: kept if it says more of the
    /// backend's time than the best before it does. A date it can't read says nothing.
    pub fn heard(&self, date: &[u8], at: Instant) {
        let Some(ms) = http_date(date) else { return };
        let heard = Dated { at, ms };
        let mut best = self.best.lock().unwrap_or_else(PoisonError::into_inner);
        // Both as of the later's arrival, after which each goes on alike: what says more then
        // says more ever after.
        let later = best.map_or(at, |best| best.at.max(at));
        if best.is_none_or(|best| best.at(later) < heard.at(later)) {
            *best = Some(heard);
        }
    }

    fn best(&self) -> Option<Dated> {
        *self.best.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The time an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`, RFC 9110's IMF-fixdate, as every
/// backend sends it) names, in milliseconds since the Unix epoch: None for what is none, or
/// before the epoch, which the clock then takes nothing from. A backend's response, untrusted
/// (`httpdate::parse_http_date` checks every field).
fn http_date(date: &[u8]) -> Option<u64> {
    let date = std::str::from_utf8(date).ok()?; // None: no date, which says nothing.
    let time = httpdate::parse_http_date(date).ok()?; // None: no date, as above.
    let since = time.duration_since(UNIX_EPOCH).ok()?; // None: before the epoch, as above.
    u64::try_from(since.as_millis()).ok() // None: out of range, as above.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The clock reads the best lower bound its responses gave, never more: a date arrived late, or
    /// one behind what the best says, changes nothing; one ahead of it, taken. Between, it goes on
    /// as the host's monotonic clock does, less [`DRIFT_PPM`]; dates it can't read, or before the
    /// epoch, say nothing.
    #[test]
    fn the_clock_keeps_the_best_lower_bound() {
        let served = Served::default();
        assert_eq!(served.now(), 0);
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        // 1994-11-06T08:49:37Z.
        let date = 784_111_777_000;
        served.heard(b"Sun, 06 Nov 1994 08:49:37 GMT", start);
        assert_eq!(served.best().unwrap().at(start), date);
        let after = |ms| served.best().unwrap().at(at(ms));
        assert_eq!(after(1_000_000), date + 1_000_000 - 500);
        // A second's date, arrived 2 s after: behind what the first says then.
        served.heard(b"Sun, 06 Nov 1994 08:49:38 GMT", at(2_000));
        assert_eq!(after(2_000), date + 2_000 - 1);
        // Dated 3 s after the first, arrived 1 s before it: 4 s more as of the first's arrival.
        let early = start.checked_sub(Duration::from_millis(1_000)).unwrap();
        served.heard(b"Sun, 06 Nov 1994 08:49:40 GMT", early);
        assert_eq!(after(0), date + 4_000);
        for nothing in [&b"yesterday"[..], b"Thu, 01 Jan 1960 00:00:00 GMT", b"\xff"] {
            served.heard(nothing, at(5_000));
            assert_eq!(after(0), date + 4_000);
        }
        // Ahead of what the best says as it arrives.
        served.heard(b"Sun, 06 Nov 1994 08:50:37 GMT", at(10_000));
        assert_eq!(after(10_000), date + 60_000);
    }
}
