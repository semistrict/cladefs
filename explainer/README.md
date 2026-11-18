# The cladefs explainer

A 132-second explainer of cladefs, made with [HyperFrames](https://github.com/heygen-com/hyperframes)
(Apache-2.0): HTML and GSAP rendered to video, frame by frame. The cells are fuzzy critters.

- `index.html` is the root: it mounts one scene per file in `compositions/` and places the audio.
- `assets/cuddly.js` draws and animates the critters, sandboxes and buckets every scene shares.
- `SPEC.md` is the look, the shots and the timings; `REVIEW.md` is what was checked.

## Preview and render

```sh
npm run dev      # the Studio preview
npm run check    # lint, layout, contrast and timing checks
npm run render   # writes renders/<name>.mp4
```

## Audio, not committed

The music and sound effects come from the HeyGen audio catalog, so they are left out of the repo
(`.gitignore`). Without them `npm run check` fails on the missing files. To restore them, sign in
(`npx hyperframes auth login`), resolve each into `.media/` and copy it to `assets/audio/` under the
name `index.html` uses:

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
ukulele, light and cuddly, gentle bounce, instrumental" --project .`. The bed is limited so the mix
reaches about -16 LUFS without clipping:

```sh
ffmpeg -i uke.wav -af "alimiter=limit=0.72:attack=2:release=60:level=false,volume=2.4dB" \
  -ar 48000 assets/audio/uke-loud.wav
```

## Marks

The Fuchsia lockup is from [fuchsia.dev](https://fuchsia.dev), and is Google's mark; it names where
Fxfs comes from. Tux is by Larry Ewing (lewing@isc.tamu.edu), made with The GIMP.
