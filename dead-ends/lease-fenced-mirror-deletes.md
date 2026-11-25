# Fencing a mirror's deletes by its pin's lease alone

**Tried:** 2026-11-22, board 8441 (`mirror.rs`'s `Pin::holding`, `PIN_HELD`); replaced by board 9229.
**Idea:** a mirror sends what its pin guards (its hold's delete in the parent's bucket, its pin's
delete, its marker's create) only within `PIN_HELD` (half of `PIN_STALE`) of a write of its pin that
landed, so that each lands before the pin goes stale and the next mirror may take it over;
`specs/buckets`' `releasesHeld` modelled it as "no late deletion".
**Result:** Codex's review (hotbed review 40, finding 1) gave the schedule: the old mirror's hold
delete, sent within its lease, stays in flight past `PIN_STALE` (cladefs's HTTP client has no
whole-request timeout, only a 20-30 s no-progress cut; a file bucket's delete waits on a blocking
`flock`); the next mirror takes the pin over, finds the hold, copies and marks; the old delete lands
and removes the next mirror's hold. `mirror::tests::late_deletes_of_mirrors_letting_go_leave_the_next_ones_hold_and_pin`
reproduces it on the simulation's bucket (red with the lease alone).
**Why it's dead:** a lease bounds when a request is *sent*, never when it *lands*: nothing bounds a
request's flight, so a lease can't fence a delete. Board 9229 replaced lease-fenced deletes with
conditional deletes (`Bucket::delete_if`), and board 11699 replaced those with per-run write-once
pin and hold keys (`<P>refs/<at>/mirror-pins/<run>`, `<P>refs/<at>/mirror-holds/<B>/<run>`) so `releasesOwn`
holds by key disjointness under plain unconditional `DELETE` on every backend. The lease still gates
the marker's create, which no condition can fence (board 9498).
**Reopen if:** never for deletes; for the marker, only if a bucket bounds a request's flight.
