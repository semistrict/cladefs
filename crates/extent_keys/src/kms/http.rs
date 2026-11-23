// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what the KMS backends over HTTP share (OpenBao, Google Cloud KMS, Azure Key Vault
//! and its token's issuers). A reply is read capped ([`body`], board 8527): one daemon serves every
//! tenant, so a hostile or broken endpoint may not have it read gigabytes, for as long as a
//! request's timeout lets it, at each of its tries. What it reads is held in a buffer zeroed when
//! dropped, the HTTP client's own chunks zeroed too where it holds them alone; a request's body is
//! one too ([`json`], [`zeroed`], board 8589). The copies reqwest, hyper and rustls make on the way
//! are theirs, which nothing here can zero.

use bytes::Bytes;
use zeroize::{Zeroize as _, Zeroizing};

/// `request` as JSON, a body whose bytes are zeroed once the client drops it: written into room
/// enough for it, so that no copy is left behind growing it.
pub(super) fn json(request: &impl serde::Serialize) -> Bytes {
    let mut body = Zeroizing::new(Vec::with_capacity(1024));
    serde_json::to_writer(&mut *body, request).expect("a request serializes");
    Bytes::from_owner(body)
}

/// `body`, as a request's, zeroed once the client drops it.
pub(super) fn zeroed(body: impl AsRef<[u8]> + zeroize::Zeroize + Send + 'static) -> Bytes {
    Bytes::from_owner(Zeroizing::new(body))
}

/// The most a KMS's reply to a wrap or an unwrap may hold, or its refusal: a key's ciphertext, or
/// its plaintext, in base64 and JSON, and an error, each a KiB or so (hotbed question 477).
pub(super) const MAX: usize = 16 * 1024;

/// The most a token's reply (Azure's authority, its instance metadata service) may hold, a JWT of a
/// few KiB; and any of OpenBao's, whose reading of a key lists each of its versions.
pub(super) const TOKEN_MAX: usize = 64 * 1024;

/// Why a reply was not read.
pub(super) enum Unread {
    /// The network failed, or timed out: another try may do better.
    Transport(reqwest::Error),
    /// It held more than this many bytes: refused, read no further.
    TooLong(usize),
}

impl Unread {
    /// Whether another try may read it.
    pub fn transient(&self) -> bool {
        matches!(self, Unread::Transport(_))
    }

    /// Why, said: a transport error with its causes.
    pub fn why(&self) -> String {
        match self {
            Unread::Transport(err) => chain(err),
            Unread::TooLong(max) => format!("a reply of over {max} bytes, refused"),
        }
    }
}

/// The body of `response`, of `max` bytes at most, refused past that, whatever it says it holds:
/// in room made for it at once, so that no copy is left behind growing it.
pub(super) async fn body(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Zeroizing<Vec<u8>>, Unread> {
    let said = response.content_length();
    if said.is_some_and(|said| said > max as u64) {
        return Err(Unread::TooLong(max));
    }
    let mut body = Zeroizing::new(Vec::with_capacity(said.map_or(max, |said| said as usize)));
    while let Some(chunk) = response.chunk().await.map_err(Unread::Transport)? {
        let fits = body.len().checked_add(chunk.len()).is_some_and(|len| len <= max);
        if fits && body.len() + chunk.len() > body.capacity() {
            // More than it said it held: moved to room for the most, the old room zeroed.
            let mut room = Zeroizing::new(Vec::with_capacity(max));
            room.extend_from_slice(&body);
            body = room;
        }
        if fits {
            body.extend_from_slice(&chunk);
        }
        if let Ok(mut unique) = chunk.try_into_mut() {
            unique[..].zeroize();
        }
        if !fits {
            return Err(Unread::TooLong(max));
        }
    }
    Ok(body)
}

/// `error` and its sources, each after the one before.
pub(super) fn chain(error: &dyn std::error::Error) -> String {
    let mut why = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        why.push_str(": ");
        why.push_str(&cause.to_string());
        source = cause.source();
    }
    why
}
