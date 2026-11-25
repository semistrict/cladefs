// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A restored VM's bucket page-in and the request that waited for it are exported together.

#![cfg(target_os = "linux")]

mod common;

use cladefs::{CellClient, CellId, ExtentId, VolatileSliceMut};
use common::daemon::{CAPACITY, Daemon, write};
use common::pattern;
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SimpleSpanProcessor};

#[test]
fn page_ins_from_the_bucket_are_traced_with_the_requests_that_waited() {
    let exporter = InMemorySpanExporter::default();
    cladefs::cladefsd::otel::install(Sampler::AlwaysOn, SimpleSpanProcessor::new(exporter.clone()));
    let dir = tempfile::tempdir().unwrap();
    let host = |name: &str| {
        let host_dir = dir.path().join(name);
        std::fs::create_dir_all(&host_dir).unwrap();
        let mut daemon = Daemon::isolated(&host_dir);
        let socket = daemon.grant(1, None).unwrap();
        (daemon, socket)
    };
    let (mut here, here_socket) = host("here");
    let vm = CellClient::connect_new(&here_socket, 1, CAPACITY).unwrap();
    let cell = CellId { tenant: 1, id: vm.cell() };
    write(&vm, &pattern(5), b"snapshotted");
    let extent = ExtentId { cell: vm.cell(), sequence: 1 };
    let version = vm.make_version().unwrap();
    drop(vm);
    here.sync(cell).unwrap();
    here.shrink_memory(None, 0).unwrap();

    let (_there, there_socket) = host("there");
    let restored = CellClient::connect_forked_cell(&there_socket, 1, version, CAPACITY).unwrap();
    let mut read = vec![0; 64 << 10];
    restored.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
    assert!(read == pattern(5), "the restored VM reads other bytes");

    let spans = exporter.get_finished_spans().unwrap();
    let page_ins: Vec<_> = spans.iter().filter(|span| span.name == "cache.page_in").collect();
    let [page_in] = page_ins.as_slice() else {
        panic!("expected one page-in, found {}", page_ins.len());
    };
    assert!(
        page_in.attributes.iter().any(|attr| {
            attr.key.as_str() == "cladefs.source" && attr.value.as_str() == "bucket"
        })
    );
    let parent = spans
        .iter()
        .find(|span| {
            span.span_context.trace_id() == page_in.span_context.trace_id()
                && span.span_context.span_id() == page_in.parent_span_id
        })
        .unwrap();
    assert_eq!(parent.name, "cladefsd.read");
    assert!(parent.attributes.iter().any(|attr| {
        attr.key.as_str() == bucket_cell::trace::BACKTRACE && !attr.value.as_str().is_empty()
    }));
}
