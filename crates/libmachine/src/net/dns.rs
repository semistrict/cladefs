// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The DNS messages the VMM's resolver reads and writes (RFC 1035): it
//! takes a standard query with one question from the guest and answers it.
//! The guest writes these bytes, so the parser trusts nothing in them.

use std::net::Ipv4Addr;

pub(crate) const TYPE_A: u16 = 1;
pub(crate) const CLASS_IN: u16 = 1;
const HEADER_LEN: usize = 12;
/// The most a reply over UDP may be without EDNS (RFC 1035, 4.2.1).
const MAX_UDP: usize = 512;
/// An answer: a pointer to the question's name, type, class, TTL, length
/// and an IPv4 address.
const ANSWER_LEN: usize = 16;

/// Response codes (RFC 1035, 4.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Rcode {
    NoError = 0,
    FormErr = 1,
    ServFail = 2,
    NxDomain = 3,
    NotImp = 4,
    Refused = 5,
}

/// A query: its header fields that the reply echoes, and its question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    pub(crate) id: u16,
    /// Recursion desired, echoed.
    pub(crate) rd: bool,
    /// The name asked, lowercase, without the root's trailing dot.
    pub(crate) name: String,
    pub(crate) qtype: u16,
    pub(crate) qclass: u16,
    /// The question as it came, echoed in the reply.
    question: Vec<u8>,
}

/// Why a message is not a query this server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Invalid {
    /// Not worth a reply: too short for a header, or itself a response.
    Drop,
    /// Answer with this code and no question.
    Reply { id: u16, rd: bool, rcode: Rcode },
}

/// Parses a query: a standard query (opcode 0) holding exactly one
/// question, whose name is a hostname (letters, digits, `-`, `_`).
/// Additional records (an EDNS OPT) are ignored.
pub(crate) fn parse_query(msg: &[u8]) -> Result<Query, Invalid> {
    if msg.len() < HEADER_LEN {
        return Err(Invalid::Drop);
    }
    let id = u16::from_be_bytes([msg[0], msg[1]]);
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    if flags & 0x8000 != 0 {
        return Err(Invalid::Drop);
    }
    let rd = flags & 0x0100 != 0;
    let reply = |rcode| Err(Invalid::Reply { id, rd, rcode });
    let opcode = (flags >> 11) & 0xf;
    if opcode != 0 {
        return reply(Rcode::NotImp);
    }
    let count = |i: usize| u16::from_be_bytes([msg[i], msg[i + 1]]);
    let (qdcount, ancount, nscount) = (count(4), count(6), count(8));
    if qdcount != 1 || ancount != 0 || nscount != 0 {
        return reply(Rcode::FormErr);
    }
    let mut at = HEADER_LEN;
    let mut name = String::new();
    loop {
        let Some(&len) = msg.get(at) else {
            return reply(Rcode::FormErr);
        };
        at += 1;
        if len == 0 {
            break;
        }
        // No compression pointers (0xc0) or extended label types in a
        // question: a query has no earlier name to point to.
        if len > 63 {
            return reply(Rcode::FormErr);
        }
        let Some(label) = msg.get(at..at + usize::from(len)) else {
            return reply(Rcode::FormErr);
        };
        at += usize::from(len);
        if !label.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_') {
            return reply(Rcode::Refused);
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.extend(label.iter().map(|b| b.to_ascii_lowercase() as char));
        // A name is at most 255 bytes in wire format (RFC 1035, 2.3.4).
        if at - HEADER_LEN > 255 {
            return reply(Rcode::FormErr);
        }
    }
    let Some(tail) = msg.get(at..at + 4) else {
        return reply(Rcode::FormErr);
    };
    let qtype = u16::from_be_bytes([tail[0], tail[1]]);
    let qclass = u16::from_be_bytes([tail[2], tail[3]]);
    at += 4;
    if name.is_empty() {
        return reply(Rcode::Refused);
    }
    Ok(Query { id, rd, name, qtype, qclass, question: msg[HEADER_LEN..at].to_vec() })
}

fn header(id: u16, rd: bool, rcode: Rcode, qdcount: u16, ancount: u16) -> Vec<u8> {
    // QR (a response), RD echoed, RA (recursion is available).
    let flags: u16 = 0x8000 | if rd { 0x0100 } else { 0 } | 0x0080 | rcode as u16;
    let mut out = Vec::with_capacity(MAX_UDP);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&qdcount.to_be_bytes());
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&[0; 4]);
    out
}

/// The reply to `invalid`, if it gets one.
pub(crate) fn error_reply(invalid: Invalid) -> Option<Vec<u8>> {
    match invalid {
        Invalid::Drop => None,
        Invalid::Reply { id, rd, rcode } => Some(header(id, rd, rcode, 0, 0)),
    }
}

/// The reply to `q`: `rcode`, and with it as many of `addrs` as A records
/// as fit in a UDP message, each with `ttl` seconds.
pub(crate) fn reply(q: &Query, rcode: Rcode, addrs: &[Ipv4Addr], ttl: u32) -> Vec<u8> {
    let room = (MAX_UDP - HEADER_LEN - q.question.len()) / ANSWER_LEN;
    let addrs = &addrs[..addrs.len().min(room)];
    let mut out = header(q.id, q.rd, rcode, 1, addrs.len() as u16);
    out.extend_from_slice(&q.question);
    for addr in addrs {
        // The name: a pointer to the question's, right after the header.
        out.extend_from_slice(&(0xc000 | HEADER_LEN as u16).to_be_bytes());
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&addr.octets());
    }
    out
}

/// A query as a stub resolver sends it, for tests and fuzz seeds.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn query_bytes(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&0x0100u16.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&[0; 6]);
    for label in name.split('.') {
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0);
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&CLASS_IN.to_be_bytes());
    msg
}

/// The answers in a reply this module wrote: (rcode, addresses).
#[cfg(test)]
pub(crate) fn answers(reply: &[u8]) -> (u8, Vec<Ipv4Addr>) {
    let rcode = reply[3] & 0xf;
    let qd = u16::from_be_bytes([reply[4], reply[5]]);
    let an = u16::from_be_bytes([reply[6], reply[7]]);
    let mut at = HEADER_LEN;
    if qd == 1 {
        while reply[at] != 0 {
            at += 1 + usize::from(reply[at]);
        }
        at += 5;
    }
    let addrs = (0..an)
        .map(|_| {
            let rdata = &reply[at + 12..at + 16];
            at += ANSWER_LEN;
            Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])
        })
        .collect();
    (rcode, addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_query_and_answers_it() {
        let msg = query_bytes(0x1234, "Example.COM", TYPE_A);
        let q = parse_query(&msg).unwrap();
        assert_eq!((q.id, q.rd, q.name.as_str()), (0x1234, true, "example.com"));
        assert_eq!((q.qtype, q.qclass), (TYPE_A, CLASS_IN));
        let addrs = [Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(5, 6, 7, 8)];
        let r = reply(&q, Rcode::NoError, &addrs, 60);
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80, "a response");
        assert_eq!(answers(&r), (0, addrs.to_vec()));
        // The question is echoed as it came, case included.
        assert_eq!(&r[12..12 + msg.len() - 12], &msg[12..]);
    }

    #[test]
    fn answers_fit_in_512_bytes() {
        let name = ["a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(61)].join(".");
        let q = parse_query(&query_bytes(1, &name, TYPE_A)).unwrap();
        let addrs = vec![Ipv4Addr::new(1, 1, 1, 1); 100];
        let r = reply(&q, Rcode::NoError, &addrs, 1);
        assert!(r.len() <= MAX_UDP, "{}", r.len());
        assert_eq!(answers(&r).1.len(), (MAX_UDP - 12 - (name.len() + 2 + 4)) / 16);
    }

    #[test]
    fn rejects_what_it_does_not_answer() {
        let reply_code = |msg: &[u8]| match parse_query(msg) {
            Err(Invalid::Reply { rcode, .. }) => Some(rcode),
            Err(Invalid::Drop) => None,
            Ok(q) => panic!("parsed {q:?}"),
        };
        assert_eq!(reply_code(&[0; 11]), None);
        let mut response = query_bytes(1, "a.b", TYPE_A);
        response[2] |= 0x80;
        assert_eq!(reply_code(&response), None);
        let mut notify = query_bytes(1, "a.b", TYPE_A);
        notify[2] |= 4 << 3;
        assert_eq!(reply_code(&notify), Some(Rcode::NotImp));
        let mut two = query_bytes(1, "a.b", TYPE_A);
        two[5] = 2;
        assert_eq!(reply_code(&two), Some(Rcode::FormErr));
        let mut pointer = query_bytes(1, "a.b", TYPE_A);
        pointer[12] = 0xc0;
        assert_eq!(reply_code(&pointer), Some(Rcode::FormErr));
        let truncated = query_bytes(1, "abc.def", TYPE_A);
        for len in 12..truncated.len() {
            assert_eq!(reply_code(&truncated[..len]), Some(Rcode::FormErr), "{len}");
        }
        assert_eq!(reply_code(&query_bytes(1, "a b.c", TYPE_A)), Some(Rcode::Refused));
        let mut root = query_bytes(1, "x", TYPE_A);
        root.remove(13);
        root[12] = 0;
        assert_eq!(reply_code(&root), Some(Rcode::Refused));
        let long = vec!["abcdefghi"; 30].join(".");
        assert_eq!(reply_code(&query_bytes(1, &long, TYPE_A)), Some(Rcode::FormErr));
        let err = error_reply(Invalid::Reply { id: 7, rd: false, rcode: Rcode::FormErr }).unwrap();
        assert_eq!(err, [0, 7, 0x80, 0x81, 0, 0, 0, 0, 0, 0, 0, 0]);
    }
}
