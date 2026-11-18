---
id: TASK-576
title: Re-attaching a cell whose store is still on the host ignores the given token
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 967000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cross-host-take-restore review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/storage/mod.rs`: `rg -n -F 'let found = local_cell::host_cell(path).map_err(Error::new)?;' crates/cladefs/src/storage/mod.rs`

In Storage::take, the FUSE mount's Attach (mount.proto: token 'as CreateCell's'), the existing-store branch only checks the cell and calls Self::open(path). KeptImage::open takes the head with the description's placed token, or the host's (Shared::start: `recovering.token().unwrap_or(self.options.token)`), never the token passed in. OpenCell::take_with by contrast places an existing store at the given token first. The mount's Detach leaves the store directory behind, so re-attaching a cell that was meanwhile attached on another host at a higher token fails ESTALE even when the caller supplies a newer, valid token, and Control records Attaching::Attached(token) for a store placed at an older token. A crash in Storage::take between opened.made() and place_host likewise leaves the store unplaced, and the retry's token is ignored.

**Trigger.** Attach(tenant, cell C, token 5) on host A, then Detach: A's stores dir keeps TENANT/C, placed at 5. Host B attaches C at token 7. Attach(C, token 9) back on A: Storage::take finds the path, open takes the head with token 5 < 7, head::take returns Error::Stale, and the attach fails ESTALE until an operator removes the directory.

**Why the tests stay green.** Mount control tests attach new cells or re-attach on the same host without an intervening higher placement. None passes a newer token for an existing store.

**Backlog.** No related task covers it: TASK-266 (Done) fixed the same token bug for CreateCell's new-store path only; TASK-302 (Done) designed Storage::take's reopen and TASK-475.2 (Done) moved the mount into the daemon.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Attach with a fencing token on a host whose stores directory still holds the cell's store, placed at an older token, takes the head at the given token: after host A attached at 5 and detached and host B took the cell at 7, Attach at 9 on A succeeds with the head's token 9; ESTALE comes only when a later placement holds the cell.
- [ ] #2 That re-attach also succeeds when B's writer committed past A's last commit, the diverged local log being dropped as OpenCell::take_with does, rather than failing ESTALE.
- [ ] #3 A regression test in the mount's control tests (fuse_mount.rs) attaches a cell at token 1, detaches, takes its head elsewhere at token 2 (bucket_cell::testing::take), attaches at 3 on the same mount and asserts success with head token 3; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
