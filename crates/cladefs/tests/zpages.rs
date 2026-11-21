// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-672: what the daemon's z-pages show of a restore on a host that holds nothing of its
//! version, every read of which goes to the bucket: on `/z/tracez`, the page-ins that went there,
//! `cladefs.source=bucket`, and the stack of the read that waited for them, resolved.

#![cfg(target_os = "linux")]

mod common;

use cladefs::cladefsd::zpages::tracez;
use cladefs::{CellClient, ExtentId, VolatileSliceMut};
use common::daemon::{CAPACITY, Daemon, write};
use common::pattern;
use std::io::{Read as _, Write as _};

/// The body of what the daemon's HTTP server at `address` answers `GET path`, which must be found.
fn get(address: std::net::SocketAddr, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: cladefsd\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    body.to_owned()
}

/// A VM snapshotted on one host and restored on another, of one process as a bucket in memory is,
/// the first host's memory emptied of what it read as the second never had it: the restored VM's
/// read pages its block in from the bucket, and `/z/tracez` shows that page-in, its source the
/// bucket, under the read, whose stack names the bring-in that waited.
#[test]
fn page_ins_from_the_bucket_show_on_tracez_with_the_stack_that_waited() {
    cladefs::cladefsd::otel::install(opentelemetry_sdk::trace::Sampler::AlwaysOn, None);
    let dir = tempfile::tempdir().unwrap();
    let host = |name: &str| {
        let acme = dir.path().join(name).join("acme");
        std::fs::create_dir_all(&acme).unwrap();
        let mut daemon = Daemon::new(&dir.path().join(name));
        let socket = daemon.grant(1, &acme, None).unwrap();
        (daemon, acme, socket)
    };
    let (mut here, here_acme, here_socket) = host("here");
    let vm = CellClient::connect(&here_socket, here_acme.join("vm"), CAPACITY).unwrap();
    write(&vm, &pattern(5), b"snapshotted");
    let extent = ExtentId { cell: vm.cell(), sequence: 1 };
    let version = vm.make_version().unwrap();
    drop(vm);
    here.sync(&here_acme.join("vm")).unwrap();
    here.shrink_memory(None, 0).unwrap();

    let (_there, there_acme, there_socket) = host("there");
    let restored =
        CellClient::connect_forked(&there_socket, there_acme.join("vm"), version, CAPACITY)
            .unwrap();
    let mut read = vec![0; 64 << 10];
    restored.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
    assert!(read == pattern(5), "the restored VM reads other bytes");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || cladefs::cladefsd::serve_http(listener));
    let page_ins = tracez::recorder().samples("cache.page_in", None);
    let source = |sample: &tracez::Sample| {
        let attributes = &sample.span().attributes;
        let source = attributes.iter().find(|kv| kv.key.as_str() == "cladefs.source");
        source.map(|kv| kv.value.as_str().into_owned())
    };
    let sources: Vec<Option<String>> = page_ins.iter().map(source).collect();
    assert_eq!(sources, [Some("bucket".to_owned())]);

    let html = get(address, "/z/tracez?name=cache.page_in&tag=cladefs.source%3Dbucket");
    assert!(html.contains("<b>cladefsd.read</b>"), "the read the page-in was for: {html}");
    assert!(html.contains("<i>cladefs.source</i>=bucket"), "{html}");
    assert!(html.contains("bring_in"), "the read's stack, resolved: {html}");
    let summary = get(address, "/z/tracez");
    assert!(summary.contains("cache.page_in"), "{summary}");
}
