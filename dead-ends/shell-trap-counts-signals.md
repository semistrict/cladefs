# Count delivered signals in the guest with a shell trap

**Tried:** 2026-11-14, board 13131, branch `batch-27-28` (never run: dropped while writing the batch's
KVM tests).
**Idea:** to show that each signal a `Process` sends arrives exactly once across vsock resets, run
`sh -c "n=0; trap 'n=$((n+1))' USR1; ...; echo $n"` in the guest, send 20 USR1s under a reset storm
and expect `20`.
**Result:** not a usable oracle. Standard signals do not queue: two USR1s arriving before the shell
runs its trap count once, and a reconnect resends the signals the agent had not taken back to back,
so a correct client can read low. busybox ash keeps a flag per signal, not a count, so real-time
signals (which do queue) do no better.
**Why it's dead:** the guest's shell cannot count signals that arrive close together. Exactly-once
signals are tested where they are counted, in the protocol: the client's unit tests
(`stdin_and_signals_are_sent_again_from_what_the_agent_took`, `what_is_sent_again`) and the agent's
`Attached { signals }`. The KVM test checks only that a signal sent across resets arrives.
**Reopen if:** the test guest gains a small counting program (e.g. a mode of `libmachine-agent`
that counts `SIGUSR1` with `signalfd` and prints the count), or the agent exposes its counters.
