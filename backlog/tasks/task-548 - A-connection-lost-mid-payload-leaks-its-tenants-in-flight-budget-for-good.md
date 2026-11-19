---
id: TASK-548
title: A connection lost mid-payload leaks its tenant's in-flight budget for good
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:24'
updated_date: '2026-11-19 07:06'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 939000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-resources, daemon-protocol review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/cladefsd/server.rs`: `rg -n -F 'self.tenant.in_flight().take(bytes).await;' crates/cladefs/src/cladefsd/server.rs`
**Where:** `crates/cladefs/src/cladefsd/server.rs`: `rg -n -F 'self.tenant.in_flight().take(bytes).await;' crates/cladefs/src/cladefsd/server.rs`

Connection::within_bounds, the admit callback AsyncReceiver::receive runs before reading a payload, charges a request's stream bytes (up to MAX_PAYLOAD, 64 MiB, plus a header) to the tenant's shared memory::InFlight and to the connection's requests.stream_bytes from the header alone. Only Answer::reply gives them back, and no Message or Answer is made for a message whose payload never arrives. When the payload read fails with UnexpectedEof, ECONNRESET or another I/O error (wire.rs, the receive loop then breaks), nothing releases the reservation: Connection::drop, finish_connection and AsyncSender::drop release only queued reply bytes. That happens in normal operation: the VM process's Sender::send_parts sends the header with sendmsg and then write_all's the payload, and a WRITE goes through the stream when it is larger than a 256 KiB transfer slot or when all 8 slots are busy (stream READs are charged too), so a VM process SIGKILLed during sandbox teardown, crashing, or let go by the watchdog under load (TASK-249's case) mid-send leaks up to 64 MiB; so does the daemon shutting the socket itself (watchdog let_go, peer.exited, Ended::Broke, drain). A compromised VM process does it at will: send a WRITE header with len up to 64 MiB, hang up, reconnect, repeat; nothing limits reconnecting. The leak lasts for the tenant's life, and the default tenant, which serves every VM on the daemon's own socket, lives as long as the daemon; other tenants are unaffected, each having its own InFlight. InFlight::try_take admits only when held == 0 or held + bytes <= budget, so once held is never 0 again and nears the budget (8 GiB by default, DEFAULT_TENANT_IN_FLIGHT: about 128 leaks at 64 MiB or 8192 at 1 MiB; a single leaked byte at the floor budget of MAX_PAYLOAD), every later stream WRITE and READ of every VM process of that tenant waits in take() forever and the guests hang; the watchdog kills one with a second-stage operation outstanding. A receive task parked in take() reads nothing more, ACKs included, and never sees EOF: when its VM process exits (peer.exited) or the daemon drains (drain::started), run_connection's went.await never returns, the connection's store hold is never let go (a restarted VM gets EBUSY after CLOSING_WAIT), and CONNECTIONS.done() never completes, so a SIGTERM drain runs to its bound and exits 1. The leak also inflates in_flight_held(), which process.rs adds to the anonymous-memory budget as a running maximum, hiding real overshoots. Tests stay green because a_tenants_in_flight_data_is_bounded (hostile.rs) checks only held and the peak while its flooding connections are alive and never ends one mid-payload, and the hostile proptest's LastWords::MissingPayload (hostile.rs and tests.rs) draws its length from any::<u64>(), which almost never (about 2^-38) falls at or below MAX_PAYLOAD plus a header, so those headers are refused before admission; no case checks that a tenant's in_flight().held() returns to 0 once its connections are gone. The verifiers rated it high (a permanent hang a hostile VM process can cause within its tenant) and medium (slow to wedge a tenant at the default budget).

**Backlog.** RELATED: TASK-149.35 (Done) made the in-flight budget per tenant, turning a per-connection leak that died with its connection into a lasting one, and never promised release when a payload never arrives; TASK-149.17 and TASK-79.16 (Done) bound other things a VM process can make the daemon hold.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a connection ends partway through an admitted payload (a WRITE header of any length up to MAX_PAYLOAD, followed by part of its payload and a hang-up or reset), its tenant's in_flight().held() returns to 0 once the connection is gone, and with the budget set to MAX_PAYLOAD a new connection's 1 MiB inline WRITE is answered within a few seconds.
- [ ] #2 A connection whose receive waits for in-flight budget ends when its VM process exits or a SIGTERM drain starts: its store hold is let go (a restarted VM process opens the store without EBUSY) and the drain completes before its bound.
- [ ] #3 A regression test in cladefsd's hostile.rs, beside a_tenants_in_flight_data_is_bounded, sends a WRITE header with len = MAX_PAYLOAD and 4 KiB of payload, shuts the socket, waits for connections() to drop, and asserts held() == 0 and that a later 1 MiB WRITE succeeds; LastWords::MissingPayload draws lengths within 1..=MAX_PAYLOAD and each hostile case ends by checking every tenant's held() is 0; the test goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
