# The cladefs explainers

Explainer films of cladefs, made with [HyperFrames](https://github.com/heygen-com/hyperframes)
(Apache-2.0): HTML and GSAP rendered to video, frame by frame. The cells are fuzzy critters on
lilac felt, over a ukulele bed, with no voice.

| Film | What it explains | Length |
|---|---|---|
| `overview/` | How cladefs stores sandbox disks: one copy, a hundred sandboxes | 132 s |
| `encryption/` | Encryption at rest, per-tenant keys, and shredding | 62 s |
| `replication/` | Replicated commits: durable on a majority of hosts before the bucket | 70 s |
| `regions/` | Many regions: cells across buckets | 72 s |
| `shared-volumes/` | Shared volumes: one checkout kept current, attached under writable layers | 72 s |
| `images/` | Container images as bases | 62 s |

Each film is a HyperFrames project of its own: `index.html` mounts one scene per file in
`compositions/`, `SPEC.md` is its look, shots and timings, and `REVIEW.md` what was checked.

`shared/` is what every film draws on, linked into each film's `assets/`:

- `shared/cuddly.js` draws and animates the critters, sandboxes and buckets.
- `shared/logos/` holds real marks (below).
- `shared/audio/` holds the music and sound effects, not committed (below).

A film's own helpers go in its own `assets/`; a helper two films use moves into `shared/cuddly.js`.

## Preview and render

From this directory, naming the film:

```sh
npm run dev -- encryption      # the Studio preview
npm run check -- encryption    # lint, layout, contrast and timing checks
npm run render -- encryption   # writes encryption/renders/<name>.mp4
```

`render` passes `--include-outside-files`, since a film's `assets/cuddly.js` links outside the
film's directory into `shared/`, and the renderer leaves out scripts outside the project otherwise.

## Audio, not committed

The music and sound effects come from the HeyGen audio catalog, so they are left out of the repo
(`.gitignore`). Without them `npm run check` fails on the missing files. To restore them, sign in
(`npx hyperframes auth login`), resolve each into `.media/` and copy it to `shared/audio/` under
the name the films use:

| File | Catalog track | What it is |
|---|---|---|
| `uke-loud.wav` | bgm `e74219cd3fa249c984a60d534b45be63` | ukulele and pizzicato, 120 BPM, 22 s; limited, below |
| `sfx_003.mp3` | `4558aa1365870399` | soft pop |
| `sfx_004.mp3` | `13175e257d71f2fa` | small hollow pop |
| `sfx_006.mp3` | `762c29b84151f93e` | short riser |
| `sfx_008.mp3` | `a9aa59d03ca9d1ab` | cartoon boing |
| `sfx_009.mp3` | `71335c7139f13308` | stutter into a squeak |
| `sfx_010.mp3` | `a4855a82116cd4cc` | airy whoosh |
| `sfx_012.mp3` | `7a8855780e5aa978` | rising chimes |

For example: `npx hyperframes media-use resolve --type bgm --intent "whimsical playful pizzicato
ukulele, light and cuddly, gentle bounce, instrumental" --project overview`. The bed is limited so
the mix reaches about -16 LUFS without clipping:

```sh
ffmpeg -i uke.wav -af "alimiter=limit=0.72:attack=2:release=60:level=false,volume=2.4dB" \
  -ar 48000 shared/audio/uke-loud.wav
```

## Marks

The Fuchsia lockup is from [fuchsia.dev](https://fuchsia.dev), and is Google's mark; it names where
Fxfs comes from. Tux is by Larry Ewing (lewing@isc.tamu.edu), made with The GIMP.
