# Giving the agent tests' reset storms quiet windows, as if a slow host starved the client

**Tried:** 2026-11-14, board 13193. It was never committed; the storm in
`crates/libmachine/tests/kvm_agent.rs` (`under_resets`) is unchanged.
**Idea:** `a_process_streams_through_resets` failed only under load, after the client's 10 s
retry budget. The storm resets every connection 0 to 8 ms apart, so an exchange that a loaded
host stretched past 8 ms looked like it could never get through. A 0–1 ms storm made the test
fail with no load at all, which seemed to confirm it. The plan was a quiet window of 150 ms,
later 400 ms, every 16 resets.
**Result:** with the windows the test failed more, not less: 10 of 32 loaded runs of the binary,
against 1 in 24 before. Timestamped logs of the reattach showed why. In every quiet window the
replay of the same 530,000 bytes of stdin started and was still blocked when the window ended.
This was a deadlock in the client, not starvation. The reader waited for the writing lock, which
the replaying writer held, and the echoing command took no stdin until its output was read
(board 13193). With that fixed and the storm unchanged, all 32 loaded runs passed.
**Why it's dead:** the storm was right. It was the only test reaching the state the bug needs
(stdin in flight and output unread at a reset), and the windows would only have diluted it. A
failure that shows up only under load can be a real race that load makes likely; measure what
the stuck call is doing before blaming the host.
**Reopen if:** a storm test fails under load, and logs show calls that progress but never fit a
gap, rather than calls that are stuck.
