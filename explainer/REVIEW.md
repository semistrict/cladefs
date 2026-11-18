# Review (cuddly cut)

## Creative calls
- The person asked for whimsy, "cuddly even", "think tribbles but not annoying, not Klingon": cells are our own fuzzy critters that multiply when forked; nothing from Star Trek.
- Added at the person's request: a sandbox is memory and a disk (S1, and S7 says a memory image is a cell too); why it beats virtio-fs (S4); where the filesystem comes from, Fuchsia (S5).
- No voiceover; titles carry the words over a ukulele bed, short pops and boings on visible actions.
- Added later at the person's request: a shared git checkout (S11), a host going away and fencing (S12), per-tenant keys and shredding (S13). Left out on request: the memory budget.
- The previous cut is kept in `.trash/v1/` and `renders/cladefs-explainer.mp4`.

## Measured
- `hyperframes check`: ok, no warnings.
- Rendered audio: -16.2 LUFS integrated, true peak -1.0 dBTP (132 s). Measured, not heard.
- Looked at: the review set (every scene start and middle, every seam, phone width) and zooms of the critters.

## Weakest shot
- S4 (32-45 s) packs two panels and a stream of trips; the virtio-fs trip pills are too quick to read one by one (marked ambient); the trip counter carries the point.

## Kept on purpose
- 8 to 55x ext4 for create, cold stat, readdir, unlink: measured under nested KVM (README "Why", backlog doc-2).
- The extent ID (cell 9, seq 1003).
