// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: how an ID is written as text (TASK-504). Every `u64` that names something (a
//! tenant, a cell, a cell, an extent's sequence number, a segment's number, an epoch, a
//! version) is written in base 32, in Crockford's digits, lower case: `0`–`9`, then the letters
//! but `i`, `l`, `o` and `u`, which read as other digits or spell words. The digits are in ASCII
//! order, so text of one width sorts as the numbers it holds do.
//!
//! Two forms: [`Padded`], [`WIDTH`] digits, leading zeros kept, as bucket keys and file names hold
//! IDs, so that listings come in the IDs' order and a name's parts are told apart by width; and
//! [`Short`], no leading zeros, as operators read and type IDs (`TENANT/CELL`). [`parse`] reads
//! either; [`parse_padded`] only the first, for names whose width is part of what they are.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::fmt;

/// The digits, by value.
const DIGITS: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The digits of a padded ID: enough for 64 bits, at 5 a digit. The first holds 4 bits alone, so
/// `fzzzzzzzzzzzz` is [`u64::MAX`].
pub const WIDTH: usize = 13;

/// An ID as keys and file names hold it: [`WIDTH`] digits, leading zeros kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Padded(pub u64);

/// An ID as operators read and type it: its digits, with no leading zeros; `0` for 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Short(pub u64);

/// The [`WIDTH`] digits of `id`, the most significant first.
fn digits(mut id: u64) -> [u8; WIDTH] {
    let mut digits = [DIGITS[0]; WIDTH];
    for digit in digits.iter_mut().rev() {
        *digit = DIGITS[(id % 32) as usize];
        id /= 32;
    }
    digits
}

fn text(digits: &[u8]) -> &str {
    std::str::from_utf8(digits).expect("the digits are ASCII")
}

impl fmt::Display for Padded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(text(&digits(self.0)))
    }
}

impl fmt::Display for Short {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = digits(self.0);
        let first = digits.iter().position(|&digit| digit != DIGITS[0]).unwrap_or(WIDTH - 1);
        f.write_str(text(&digits[first..]))
    }
}

/// The value of `digit`, if it is one: lower case alone, as IDs are written.
fn value(digit: u8) -> Option<u64> {
    DIGITS.iter().position(|&each| each == digit).map(|value| value as u64)
}

/// The ID `text` writes, padded or short: 1 to [`WIDTH`] digits, a value of 64 bits. Anything
/// else, an empty text, a digit not of [`DIGITS`] (upper case among them), or a value past
/// [`u64::MAX`], is none.
pub fn parse(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > WIDTH {
        return None;
    }
    text.bytes().try_fold(0u64, |id, digit| id.checked_mul(32)?.checked_add(value(digit)?))
}

/// The ID `text` writes padded, [`WIDTH`] digits exactly; anything else is none.
pub fn parse_padded(text: &str) -> Option<u64> {
    (text.len() == WIDTH).then(|| parse(text)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn what_is_no_id_is_refused() {
        for wrong in ["", "00000000000000", "g000000000000", "zzzzzzzzzzzzz", "A", "i", "l", "o"] {
            assert_eq!(parse(wrong), None, "{wrong:?}");
        }
        for wrong in ["u", "-1", "+1", " 1", "1/2", "é"] {
            assert_eq!(parse(wrong), None, "{wrong:?}");
        }
        assert_eq!(parse("fzzzzzzzzzzzz"), Some(u64::MAX));
        assert_eq!(parse_padded("1"), None);
        assert_eq!(parse_padded("000000000001"), None);
        assert_eq!(parse_padded("00000000000001"), None);
        assert_eq!(parse_padded("0000000000001"), Some(1));
    }

    proptest! {
        #[test]
        fn both_forms_read_back(id: u64) {
            prop_assert_eq!(parse(&Short(id).to_string()), Some(id));
            prop_assert_eq!(parse(&Padded(id).to_string()), Some(id));
            prop_assert_eq!(parse_padded(&Padded(id).to_string()), Some(id));
            prop_assert_eq!(Padded(id).to_string().len(), WIDTH);
        }

        #[test]
        fn padded_ids_sort_as_numbers(a: u64, b: u64) {
            prop_assert_eq!(Padded(a).to_string().cmp(&Padded(b).to_string()), a.cmp(&b));
        }
    }
}
