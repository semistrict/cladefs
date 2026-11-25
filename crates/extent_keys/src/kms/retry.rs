// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: how the KMS backends over HTTP (Google Cloud KMS, Azure Key Vault, Azure's token
//! endpoints, and OpenBao) try a call again (board 8473, board 8475, board 9386). What failed as
//! the network does, or as the service throttling (429) or failing (5xx), is tried again,
//! [`BACKOFF`]'s waits growing between tries, each drawn at random within its upper half, so that
//! calls refused together, a pool's refill of a batch of wraps, do not all come back together; and
//! none shorter than a `Retry-After` the service sent, honoured up to [`RETRY_AFTER_MAX`] a wait
//! (hotbed question 458).

use std::future::Future;
use std::time::Duration;

/// The longest each wait before a retry may be: a call is tried `BACKOFF.len() + 1` times in all,
/// some 15 s at most between its first try and its last when the service says no `Retry-After`.
pub(super) const BACKOFF: [Duration; 4] = [
    Duration::from_millis(200),
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(10),
];

/// The longest wait a `Retry-After` is honoured for: one longer is waited this long, then the call
/// tried again, so that a call waits a minute or so at most, not what the service asks.
pub(super) const RETRY_AFTER_MAX: Duration = Duration::from_secs(30);

/// Whether an HTTP status says the service may answer the same request next time: it throttled it
/// (429) or failed (5xx), whatever the body says.
pub(super) fn transient_status(code: u16) -> bool {
    code == 429 || (500..=599).contains(&code)
}

/// The wait a `Retry-After` header of `headers` asks for, if it says a number of seconds: an HTTP
/// date, which neither Cloud KMS nor Key Vault sends, is not honoured, the backoff's wait taken.
pub(super) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    // Not text, or not a number of seconds: none honoured, the backoff's wait taken.
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    // Not a number of seconds: none honoured, the backoff's wait taken.
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// What to wait before retry `retry`, 0 the first, `after` the service's `Retry-After` if it sent
/// one: none once the tries are spent. The backoff's wait is drawn between half of
/// [`BACKOFF`]'s and all of it; a `Retry-After`, capped at [`RETRY_AFTER_MAX`], has up to half of
/// it added, so that callers told one time come back spread.
pub(super) fn wait(retry: usize, after: Option<Duration>) -> Option<Duration> {
    let most = *BACKOFF.get(retry)?;
    let jitter = below(most / 2);
    Some(match after {
        Some(after) => after.min(RETRY_AFTER_MAX) + jitter,
        None => most / 2 + jitter,
    })
}

/// A duration drawn at random from zero to `most`, both included.
fn below(most: Duration) -> Duration {
    let mut bytes = [0; 8];
    crate::random::fill(&mut bytes);
    let most = u64::try_from(most.as_nanos()).unwrap_or(u64::MAX);
    match most.checked_add(1) {
        Some(span) => Duration::from_nanos(u64::from_le_bytes(bytes) % span),
        None => Duration::from_nanos(u64::from_le_bytes(bytes)),
    }
}

/// What a try of a call failed with: its error, whether trying again may do better, and the wait
/// the service asked for, if it did.
pub(super) struct Failed<E> {
    pub error: E,
    pub transient: bool,
    pub after: Option<Duration>,
}

impl<E> Failed<E> {
    /// `error`, which no try again may change.
    pub fn refused(error: E) -> Self {
        Self { error, transient: false, after: None }
    }

    /// `error`, which a try again may not have, after `after` if the service said.
    pub fn transient(error: E, after: Option<Duration>) -> Self {
        Self { error, transient: true, after }
    }
}

/// What `attempt` answers, tried again after [`wait`]'s waits while it fails as it may not next
/// time: the last failure's error once the tries are spent.
pub(super) async fn retried<T, E, F: Future<Output = Result<T, Failed<E>>>>(
    mut attempt: impl FnMut() -> F,
) -> Result<T, E> {
    let mut retry = 0;
    loop {
        let failed = match attempt().await {
            Ok(answer) => return Ok(answer),
            Err(failed) => failed,
        };
        match failed.transient.then(|| wait(retry, failed.after)).flatten() {
            Some(wait) => tokio::time::sleep(wait).await,
            None => return Err(failed.error),
        }
        retry += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each retry's wait is within the upper half of its backoff's, or a `Retry-After`'s, capped,
    /// plus at most half the backoff's; the tries end after the last. A `Retry-After` of an hour
    /// is waited [`RETRY_AFTER_MAX`]; one of none, the jitter alone.
    #[test]
    fn waits_grow_are_jittered_and_honour_retry_after_bounded() {
        for (retry, &most) in BACKOFF.iter().enumerate() {
            for _ in 0..64 {
                let waited = wait(retry, None).unwrap();
                assert!(most / 2 <= waited && waited <= most, "{retry}: {waited:?}");
                let after = wait(retry, Some(Duration::from_secs(3600))).unwrap();
                assert!(
                    RETRY_AFTER_MAX <= after && after <= RETRY_AFTER_MAX + most / 2,
                    "{retry}: {after:?}"
                );
                let none = wait(retry, Some(Duration::ZERO)).unwrap();
                assert!(none <= most / 2, "{retry}: {none:?}");
            }
        }
        assert_eq!(wait(BACKOFF.len(), None), None);
        assert_eq!(wait(BACKOFF.len(), Some(Duration::ZERO)), None);
        assert_eq!(below(Duration::ZERO), Duration::ZERO);
        let headers = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            headers
        };
        assert_eq!(retry_after(&headers("7")), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(&headers("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }
}
