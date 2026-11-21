# SPEC: One key at the top (encryption at rest and shredding)

GOAL:    the viewer leaves knowing that every cell is encrypted, always; that a tenant's data rests as
         ciphertext on the host's disk, on peers' disks and in the bucket, plain only in memory; that a
         chain of keys holds it (the cell DEK, locked by the cell KEK, locked by the tenant KEK in a
         KMS); and that destroying the tenant's KEK leaves its cells unreadable at rest, other tenants
         untouched (forks into other tenants' cells keep what they inherited).
OUTPUT:  HyperFrames, 1920 x 1080, 30 fps, 62 s (1860 frames).
MUSIC:   the overview's catalog ukulele bed, 120 BPM (15 frames a beat, a bar 2 s), phrase of 40 beats
         (20 s); `uke-loud.wav` (22 s: the phrase, then its last chord at 20.0 s ringing out) at 0.75,
         played at 0 and 20 for 20 s, then whole at 40, so its own last chord lands at 60.0 s, on
         tenant 7's landing.
SEAMS:   0, 8, 20, 32, 40, 54, 62 s; every seam on a bar.
LOOK:    the overview's, unchanged: ground #EFEAF8 with 3% felt noise; ink #2E2440, 4-6 px, round joins,
         corners 14-40 px; critter colours peach #FFB088, rose #FF9EC0, lilac #B9A4FF, mint #8FDDC2,
         butter #FFD86E, sky #8CC8FF; secondary text #5E5672. Nunito 900 80 px lines (at most 42
         characters), 800-900 28-40 px labels, 28 px floor; JetBrains Mono for code, hex and commands.
         Accent #E2177E only on tenant 42's KEK and the padlock it closes.
         Keys and locks are colour-matched: a padlock is the colour of the key that opens it. The cell
         DEK is butter, the cell KEK sky, tenant 42's KEK accent, tenant 7's KEK mint.
CHARACTERS: cell 9 is the overview's peach critter (seed 909); tenant 42's other cells rose and
         lilac, tenant 7's mint and sky. Sandboxes are the overview's windows with faces. The bucket is
         the overview's pail, labelled "bucket". Hosts and peers are dashed rounded boxes, disks the
         overview's lilac disk bar. A file is a card: plain, it shows code; encrypted, it shows hex and
         wears a padlock. Shredded critters go poof into dashed ghosts (overview S13).
MOTION:  the overview's: pops back.out(2.2-2.8) from the feet; hops squash, stretch, land on
         elastic.out(1, 0.4); titles pop word by word (back.out(2.2), 60 ms) and then hold still; a card
         flips (scaleX to 0 in 4 frames, face swapped, back in 4); a padlock drops on with back.out(3);
         the KEK shakes 6 times then breaks in two, falling on power2.in.
SHOTS:   S1 0-8  "Tenant 42's sandbox saves a file" on screen from frame 0; the sandbox bounces in; a
            `main.rs` card (`fn main() {` / `serve();` / `}`) pops out of it. "Its disk is kept in cell
            9": cell 9 pops in and the card hops into it. "Where is that file stored?"
         S2 8-20 "First, the host's own disk": a copy of the card drops from host memory to the host's
            disk and flips to hex as it lands ("encrypted before it is written"). "Then peers' disks, if
            replicated": copies hop to peer B's and peer C's disks ("peers keep it with no key"). "Then
            the bucket": a copy floats into the pail. "Wherever it's stored, it's encrypted": note
            "AES-256-XTS as written; framed parts GCM, or the XTS sectors tagged". The card in host
            memory stays plain all along ("host memory: plain, for the sandbox").
         S3 20-32 One diagram that grows, left to right. "The cell DEK unlocks main.rs": the encrypted
            card with a butter padlock; the DEK slides in, the card flips plain, then back ("each
            object under its own subkey of the DEK"). "The cell KEK locks the DEK": a sky padlock drops
            on the DEK, the KEK pops beside it; "cell 9's head keeps both, locked". "Tenant 42's KEK
            locks the cell KEK": an accent padlock on the KEK; the KMS pops at right holding tenant 42's
            KEK. "Every key below hangs on that one": the dotted links pulse top to bottom; note "taking
            cell 9 asks the KMS once".
         S4 32-40 "The tenant KEK lives in a KMS": in the KMS box, three rows named by URL, OpenBao
            (`openbao://…`), AWS KMS (`aws-kms://…`), Google Cloud KMS (`gcp-kms://…`), under one
            "in progress" tag; then "or the bucket KMS" (`CLADEFS_MASTER_KEY`; "the default: tenants'
            KEKs in the bucket, under a master key"). "Without one, a host refuses to start": a host box
            ("a host given neither"), the command with its bucket and token set
            (`$ CLADEFSD_BUCKET=s3://b CLADEFSD_TOKEN=8 cladefsd`), then cladefsd's own error
            (`Error: the host's KMS: no KMS: CLADEFS_KMS names none, …`), a "refuses to start" tag and a
            frowning sandbox; note "there is no plaintext mode".
         S5 40-54 "Tenant 7 lives on the same hosts": tenant 42's cells (cell 9 and two more), tenant 7's
            two; the KMS holding both KEKs; the host's disk, the peers' disks and the bucket each
            holding one card of each tenant, encrypted. "To shred tenant 42, destroy its KEK": the
            accent key shakes and breaks, its links drop. "Its cells' copies stay ciphertext": tenant
            42's cards grey under a cross; "its cell KEKs unwrap no more, once old versions expire".
            "Every host told forgets them": tenant 42's cells poof into ghosts; two notes held to the
            end: "the bucket KMS: `cladefsctl shred-tenant TENANT`, on every host" and "a KMS by URL:
            destroy the KEK there, then `forget-tenant-keys TENANT`". "Tenant 7 reads on, untouched":
            tenant 7's cards flip plain, its cells hop.
         S6 54-62 "One key gone, its cells unreadable" held to the end; ghost cell 9 beside its
            encrypted `main.rs` and the broken key; notes "stored on every host's disk, every peer's,
            and in the bucket" and "except forks into other tenants' cells; under the bucket KMS, expire
            old versions"; tenant 7's critter hops into view at 58.3 s, then beside its plain `app.py`
            card, landing on the last chord (60.0 s).
SOURCES: (doc-16 is docs/design/doc-16; ops is docs/operating-a-host.md "Encryption and shredding")
         S1 a sandbox's disk is kept in a cell: CONTEXT.md "Cell" (a tenant's data of one kind; _Avoid_
            "disk" for a cell, hence "kept in"), overview S7; tenant: CONTEXT.md "Tenant". Tenant 42,
            tenant 7 and `main.rs`'s code are illustrative.
         S2 host's disk encrypted, open segment AES-256-XTS per 4 KiB sector, closed segments and part
            cache hold framed (encrypted) bytes: doc-16 "What is encrypted" (Local disk), ops ("the
            bucket, the stores' directories and the part cache hold its data encrypted"). Framed parts:
            AES-256-GCM (doc-16 "What is encrypted", Data parts), or, for a segment framed in place, its
            XTS sectors kept as they are, each part tagged (doc-16 "What TASK-411.23 made";
            extent_frame `Seal::Sectors`). Peers: doc-21 "What a member holds" (records written
            unopened, XTS ciphertext, "needs no key"); replicated cells only (TASK-537.3.1, .3.2 Done).
            "bucket", never "S3": CONTEXT.md "Bucket" (_Avoid_ S3). Memory plain: doc-16 "What is
            encrypted" (Memory) and "Not a goal". Hex is illustrative.
         S3 tenant KEK, cell KEK, cell DEK, subkeys per object: doc-16 "Keys", CONTEXT.md "Encryption",
            crates/extent_keys/src/lib.rs (module doc), crates/bucket_cell/src/keys.rs (`Kek`, `Dek`,
            `wrap_dek`); both kept wrapped in the head: crates/bucket_cell/src/head.rs (`keks`,
            `deks`), CONTEXT.md "Head" ("its keys"). One KMS call per taking: doc-16 "Keys" (Cell
            KEK), keys.rs module doc ("Taking a cell unwraps its KEK with one call of the KMS").
         S4 the backends: README "Encryption at rest, always", ops, crates/extent_keys/src/kms/
            {openbao,aws,gcp,bucket,master}.rs. "in progress": TASK-411.16 (OpenBao; tested against a
            `bao server -dev` on macOS, Linux and CI open), TASK-411.17 (AWS KMS; tested against the
            local-kms emulator, the non-env credential paths and CI open), TASK-411.18 (Google Cloud
            KMS; tested against Cloud KMS itself, the ADC path and Linux open), all In Progress. The
            bucket KMS: TASK-411.27 Done; the default: ops ("The bucket KMS ..., the default"). No KMS
            and no master key refuses to start: TASK-411.28 (Done) AC #2, doc-16 "One sealed mode".
            The command: crates/cladefs/src/bin/cladefsd.rs `main` reads `Host::from_env` (needs
            `CLADEFSD_BUCKET` and `CLADEFSD_TOKEN`, not 0: ops) before `cladefs::bucket::require_kms`,
            so both are set for the KMS to be what fails; the bucket URL and token are illustrative.
            The message: crates/bucket_cell/src/bucket_kms.rs `no_kms()`, prefixed "the host's KMS: " by
            `require_kms`, returned from an anyhow `main` (hence "Error: "). Azure Key Vault is left
            out: TASK-633.6 is In Progress.
         S5 shredding a tenant: under the bucket KMS, `cladefsctl shred-tenant TENANT` (`ShredTenant`)
            against every host's daemon, which tombstones the tenant's KEK object; under a KMS named by
            URL, destroy the KEK there, then `cladefsctl forget-tenant-keys TENANT` (`ForgetTenantKeys`)
            on every host: crates/cladefs/src/bin/cladefsctl.rs module doc, ops, doc-16 "Shredding"
            ("Absolute: every cell KEK of it, in every version of every cell object, unwraps no more").
            Tenant IDs are base 32 there, so the film shows the documented placeholder. "once old
            versions expire": ops (a versioned bucket keeps `tenants/<tenant>/kek`'s old versions,
            which unwrap, until a lifecycle rule expires them). "Every host told forgets them": doc-16
            "What TASK-411.7 made" (a host told halts the stores and forgets; what stays: a halted
            store's own ring and its open segment's XTS key until closed; a host not told reads on
            what it has open, opening none afresh). Other tenants read on: crates/local_cell/src/upload/
            encrypted.rs `shredded_tenants_leave_none_of_their_cells_readable_on_any_host`. Not
            claimed: memory cleared at once (TASK-630, To Do).
         S6 scoped to "its cells" and "at rest": doc-16 Goals 2 and "What TASK-411.7 made" ("a fork of
            the shredded tenant into another's keeps its keys: shredding a parent leaves its children
            what they inherited"; doc-16 "Forks and shared points"; crates/bucket_cell/src/keys.rs
            inherited DEKs); the versions exception: ops, as for S5.
BANNED:  the overview's list: an end card or wordmark; the word "VM" on screen (say "sandbox"); libc as
         the example; calling cladefs "a page cache"; any franchise's creatures; dark void, glow,
         gradients, glass, everything centred, bullet lists, icons in circles, legends. This film adds:
         "snapshot" (say "version"), "sealed" for encryption (say "encrypted"), "customer"/"account"
         for a tenant; claiming memory is wiped by a shred (TASK-630); Azure Key Vault or KMS per
         bucket shown as shipped (TASK-633.6); OpenBao, AWS KMS or Google Cloud KMS without "in progress"
         (TASK-411.16/.17/.18); "S3" for the bucket and "disk" for a cell (CONTEXT.md _Avoid_); "every
         copy unreadable" unscoped (forks into other tenants keep what they inherited); a padlock in a circle badge; a key icon floating
         alone in the middle of an empty frame.
QA:      frame 0 shows the first line; S2 middle shows four places, one plain card in memory and three
         hex copies; S3 end shows the whole chain with three colour-matched padlocks; S5 at 49 s shows
         ghosts for tenant 42 and tenant 7's cards plain; the last frame holds the line, ghost cell 9
         and tenant 7's critter landed.
