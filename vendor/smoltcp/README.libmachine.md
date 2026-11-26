# smoltcp 0.12.0, patched for libmachine

The crate as published on crates.io (0BSD), with one addition: exporting
and importing a TCP socket's whole state, so that libmachine can write a
suspended guest's connections to a file and restore them (board 13209).

Changes from the published crate:

- `src/socket/tcp/snapshot.rs` (new): `SocketSnapshot`, plain data of every
  field of `tcp::Socket`, with points in time relative to a given `now`;
  `Socket::snapshot(now)` and `Socket::restore(&snapshot, now)`.
- `src/socket/tcp.rs`: the `mod snapshot` line and its re-exports.
- `src/storage/ring_buffer.rs`: `RingBuffer::peek_unallocated`, a read-only
  read of bytes written past the allocated ones (out-of-order data).
- `Cargo.toml`: `[lints.rust] warnings = "allow"`, so that the crate's own
  warnings with libmachine's features do not fill libmachine's build output
  (a path dependency's warnings are shown, unlike a registry crate's).
- `.cargo_vcs_info.json` and `.cargo-ok` removed.

The patch is kept apart so that it can be offered upstream. Nothing has
been posted upstream.
