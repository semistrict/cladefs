# Storylines: encryption at rest and shredding

The one idea: every byte of a tenant's data rests encrypted, under a chain of keys that ends at one
key in a KMS; destroy that key and the tenant's data is unreadable wherever it rests, while other
tenants read on. The worked example, in every storyline: tenant 42's sandbox saves `main.rs` into its
disk, cell 9 (the overview's peach critter), and tenant 7 lives beside it.

**Recommended: A, "One key at the top"**, because it is the only one whose shred lands on the same
copies the viewer watched being made, so "unreadable wherever it rests" is seen, not told.

## A. One key at the top (built)

- **Device and form:** the object is the explanation, as one diagram that grows; the same felt world
  as the overview, with cuts between scenes on the bar. `main.rs` leaves the sandbox, lands as
  ciphertext on the host's disk, on peers' disks and in the bucket; a chain of padlocked keys grows
  from the file up to tenant 42's KEK in a KMS; the KEK breaks, and every copy stays scrambled while
  tenant 7's copies still open. The viewer feels the weight of one small key.
- **First two seconds:** "Tenant 42's sandbox saves a file" on screen at frame 0; the sandbox with a
  face bounces in, and a `main.rs` card with real code pops out of it.
- **Beats:** a sandbox saves `main.rs` → it lands in cell 9 → copies rest as ciphertext on the host's
  disk, two peers' disks and the bucket → the DEK opens it, the cell KEK locks the DEK, tenant 42's KEK
  in the KMS locks the cell KEK → which KMS (OpenBao, AWS KMS, Google Cloud KMS, or the bucket under a
  master key); with none a host refuses to start → destroy tenant 42's KEK: every copy stays
  ciphertext, every host forgets its keys, tenant 7 reads on → "One key destroyed, its data unreadable
  wherever it rests", tenant 7's critter lands on the last chord.
- **Look:** the overview's: lilac felt, plum ink, Nunito; keys are the overview's keys with colour-
  matched padlocks; the accent only on tenant 42's KEK.
- **Sound:** the overview's ukulele bed, three passes, the last chord on the final hop; pops on keys
  and locks, a riser into the break, poofs on the shred.

## B. Where is main.rs?

- **Device and form:** a question with the input, as a flight through one world. The camera follows
  one card of `main.rs` across a long felt map (sandbox, host's disk, a peer, the bucket); at each stop a
  stranger critter (a curious peer, the bucket's pail) tries to read it and sees only hex. The keys are
  revealed last, as the reason nobody could read it, and the shred is the camera pulling back over the
  whole map as every card on it goes grey at once.
- **First two seconds:** a sticky note "where does main.rs go?" slaps onto a sandbox; the camera is
  already moving.
- **Beats:** the question → stop 1 host's disk (hex) → stop 2 a peer (hex, it holds no key) → stop 3
  the bucket (hex) → the keys revealed in one pan up → pull back, shred → the map grey but tenant 7's
  trail.
- **Look:** the same felt, one 5760 px wide map, the camera's 1920 px window travelling it.
- **Sound:** the bed, a whoosh on each leg of the flight.

## C. Russian dolls (the bold one)

- **Device and form:** match cuts on nested boxes. The data is a toy in a box; the box's key sits in a
  bigger locked box (the cell's head); that box's key in a bigger one still (the KMS). Each cut pulls
  back to reveal the next box, the same square shape holding steady while its scale changes. The shred
  crushes the outermost box, and the camera dives back through every box, each now with no way in.
- **First two seconds:** a tiny `main.rs` toy drops into a box that slams shut with a padlock click.
- **Beats:** toy in a box (DEK) → pull back: the DEK in a box (cell KEK) → pull back: in the KMS
  (tenant KEK) → copies of the outer boxes scattered on disks and the bucket → the outer box crushed →
  dive back in: every lock unopenable → tenant 7's dolls still open.
- **Look:** the same felt and critters; boxes as felt pouches with plum stitching.
- **Sound:** the bed; a click on each lock, a crunch on the crush.

Not built: B (a flight spends its seconds travelling, and the keys come too late to carry the shred)
and C (nesting boxes say "wrapped" well but hide where the copies rest, which is half the idea).
