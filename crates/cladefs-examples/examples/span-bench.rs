// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a span costs the daemon (TASK-258, TASK-672.1): a bucket request's, started, three
//! attributes set, ended, as `bucket_cell::trace` makes it; off, as a process that records no spans
//! makes it, and sampled, with the span processor sending each span to a discard exporter.
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example span-bench [SPANS]
//! ```
//!
//! SPANS is how many spans each round makes (default 1 000 000). Prints the fastest of 5 rounds'
//! time per span.

#[cfg(target_os = "linux")]
fn main() {
    use bucket_cell::trace::{self, BYTES, OBJECT_KEY};
    use opentelemetry_sdk::trace::{Sampler, SimpleSpanProcessor, SpanData, SpanExporter};
    use std::time::Instant;

    #[derive(Debug)]
    struct Discard;
    impl SpanExporter for Discard {
        async fn export(&self, _: Vec<SpanData>) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }
    }

    let spans: u32 = std::env::args().nth(1).map_or(1_000_000, |spans| spans.parse().unwrap());
    let request = || {
        let span = trace::Span::start("bucket.get");
        span.set_with(OBJECT_KEY, || "tenants/1/cells/1/segments/1.0".to_owned());
        span.set_count(BYTES, 4096);
        span.result(&Err::<(), _>("failed"));
    };
    let fastest = || {
        let round = || {
            let started = Instant::now();
            for _ in 0..spans {
                request();
            }
            started.elapsed() / spans
        };
        (0..5).map(|_| round()).min().unwrap()
    };
    println!("a span off costs {:?}", fastest());
    cladefs::cladefsd::otel::install(Sampler::AlwaysOn, SimpleSpanProcessor::new(Discard));
    println!("a span sampled costs {:?}", fastest());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("span-bench: the daemon's spans are Linux's");
}
