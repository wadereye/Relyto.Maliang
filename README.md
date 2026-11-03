| English | [简体中文](./README.zh-CN.md) |

# Relyto.Maliang (马良)

> A local photo management app that inspired by Picasa 3 with AI.

**Why this project exists.** Google Picasa was a truly great photo
management and editing app — fast, simple, and a joy to use. Sadly, Google
discontinued it in 2016, leaving a gap that no modern tool has quite filled.
That's why this project sets out to inspired by Picasa 3 in Rust and release it as
open source, so this classic experience can live on.

If Maliang is useful to you, please consider giving the repo a
[⭐ star](https://github.com/wadereye/Relyto.Maliang/stargazers) — and if
something is missing or broken, open an
[issue](https://github.com/wadereye/Relyto.Maliang/issues). Feature requests
are always welcome.

Maliang (马良) is a desktop-first local photo management tool that aims to
modernize the classic Picasa 3 experience: smoother browsing, smarter
organization, and native, offline, privacy-first AI capabilities. All of your
data stays on your own machine — nothing is uploaded to the cloud, and your
private photos are never scanned and sent anywhere.

Version 0.1 was released on 2026-10-31, and v0.2 is wrapping up. See the
[changelog](./CHANGELOG.en.md) for what's new in each release.

## Feature Overview (gradually implemented)

### Browsing & Organizing

- **Full-disk scan & index**: once a folder is added to the library, a local
  index (SQLite) is built automatically; folders come first — your originals
  are never copied or moved.
- **Multiple views**: timeline (grouped by capture date) / folders / albums
  (named collections that never touch files).
- **Left sidebar directory tree**: a real, recursively expanded tree with a
  scan progress percentage.
- **Multi-level thumbnails**: generated and cached on demand at four sizes —
  96 / 256 / 512 / 1024 — keeping browsing smooth.
- **Automatic updates**: while running, changes are tracked live via file
  system events; files added or removed while the app was closed are
  reconciled automatically at the next launch.
- **Slideshow**: play the selected photos or the entire current view.
- **Sorting & thumbnail size**: by capture date / filename, ascending or
  descending, normal / small.

### Quick Search

- **Whole-disk filename index**: the first use builds a name index for the
  entire disk (a few seconds for millions of files); afterwards, searching
  for photos, videos, or documents returns results in milliseconds.
- **Filter by type**: all / images (RAW included) / videos / audio /
  archives / executables / documents.
- **Top bar global search box**: type a filename anywhere to jump into
  quick search.
- **One-time authorization, silent background updates**: once registered as
  a scheduled task with administrator authorization, the index stays fresh
  automatically at boot; it can be revoked anytime under Options > General.

### Editing & Effects

- **Preview editing**: rotate, crop, zoom & pan, curves, exposure, contrast,
  color temperature, tint, sharpen, blur, grain, vignette, and more — all
  non-destructive; changes are only baked to disk via F2 save.
- **One-click effects**: auto contrast / auto color / warm tone / cool tone /
  positive film / LOMO / backlight compensation and dozens more, aligned
  with the original Picasa's behavior.
- **Style filters**: dozens of grouped recipes — film grain, toy camera,
  instant photo, cinemascope, sixties, pencil sketch, cross process,
  two-tone, neon, night vision, mat board, rounded corners…
- **Shader effects**: hexagonal cells, block ripples, circular splatter,
  crystallize, waterfall, per-channel HSL.

### AI Capabilities (the `ai` feature is enabled by default, can be trimmed off)

- **OCR text recognition**: text inside photos becomes searchable.
- **Face organizing**: detection, clustering, quality scoring, semi-automatic
  naming suggestions, ignored clusters.
- **CLIP semantic search**: bilingual Chinese/English descriptive search such
  as "find photos with cats".

### Import & Export

- **WiFi phone import**: visit the computer's address from a phone browser
  and send photos into Incoming.
- **Device import**: import from USB-connected cameras, phones, and other
  portable devices via WPD / WIA (Windows).
- **Export & baking**: export by the current view range with edit parameters
  baked into the output; the original files are never touched.
- **Video**: recognizes common formats, extracts thumbnails, and plays them
  with the system player.

### Management & Interoperability

- **Duplicate photo detection**: perceptual hashing finds visually similar
  photos (beyond the original's capabilities).
- **Color labels / star ratings**: reads and writes the classic `picasa.ini`,
  interoperable with the original Picasa.
- **XMP mirroring**: edits and metadata are mirrored to XMP for
  interoperability with other tools.
- **EXIF / GPS**: reads capture parameters and coordinates, with KML
  geotag export.
- **Folder manager**: add/remove watched directories, rescan each one, and
  clean up missing files.
- **Index backup & compaction**; **relocatable data directory** (thumbnails /
  index / models all move together).

### Interface & Experience

- Dark / light themes; built-in Alibaba PuHuiTi Chinese font; icons use the
  open-source Lucide set.
- Menu structure follows Picasa 3 (File / Edit / View / Tools / Help);
  native right-click menus on photos.
- Right info drawer: properties / tags / people / places.
- **Fully local: no network access, no telemetry.**

### Intentionally Not Planned (present in the original Picasa, but not planned for now)

Advanced retouching workflows (retouch / picnik), collage / poster / CD
burning, online sharing (Picasa Web / social platforms / email / printing),
TWAIN camera scanning, cloud album sync, and Picasa2 migration.

## Tech Stack
- **Language**: Rust 2021
- **UI**: [egui](https://github.com/emilk/egui) / [eframe](https://github.com/emilk/egui) 0.33 (wgpu rendering)
- **Database**: [rusqlite](https://github.com/rusqlite/rusqlite) (bundled SQLite)
- **System integration**: the `windows` crate (WPD / WIA / COM on Windows)
- **Imaging**: `image` 0.25 (JPEG / PNG / WebP / TIFF / BMP / GIF)

## Building

### Requirements
- Windows 10/11 (the project currently targets Windows only, relying on the
  `windows` crate for device import)
- The [Rust toolchain](https://www.rust-lang.org/tools/install) (stable; a
  recent version is recommended)
- Network access is required during the build to fetch dependencies from
  crates.io

### Standard Build
```bash
# Clone (if needed)
git clone https://github.com/wadereye/Relyto.Maliang
cd Relyto.Maliang

# Debug build
cargo build

# Release build (recommended; enables LTO and a single codegen unit)
cargo build --release

# Run
cargo run            # or run target/release/maliang.exe
```

### Slim Build (without AI)
The `ai` feature pulls in ONNX Runtime and model files (OCR / faces / CLIP)
and is **enabled by default**:
```bash
cargo build --release          # default = includes ai
```
If you don't want the extra size and dependencies:
```bash
cargo build --release --no-default-features
```
In a slim build, the "People" and "Semantic Search" views will indicate that
they were not compiled in; everything else works as usual. Before enabling the
feature, make sure the `onnxruntime` runtime and the corresponding model
weights are in place.

### Common Commands
```bash
cargo check          # type checking only (fast)
cargo test           # run tests (includes AI module tests by default)
cargo test --no-default-features   # tests for the slim build
```

### Build Troubleshooting (Windows)
- The project depends on the `windows` crate and COM, so use the native MSVC
  toolchain provided by the official Rust installer.
- If the C drive is short on space and `cargo`'s temporary files cause errors,
  point the temp directory elsewhere:
  ```powershell
  $env:TMP = "D:\tmp-maliang"; $env:TEMP = "D:\tmp-maliang"
  cargo build --release
  ```
- The first build compiles a large number of dependencies and takes a while —
  please be patient.

## Project Layout
```
Relyto.Maliang/
├── Cargo.toml          # package definition and dependencies
├── Cargo.lock
├── assets/
│   └── presets.dat     # built-in effect parameter pack (embedded at compile time, overridable via CLI)
├── src/
│   ├── main.rs         # entry point (UI thread DPI awareness, index store, eframe startup)
│   ├── app.rs          # main UI (views / sorting / folder manager)
│   ├── scan.rs         # directory scanning and indexing
│   ├── thumbs.rs       # thumbnail generation and caching
│   ├── timeline.rs     # timeline aggregation
│   ├── watch.rs        # directory change monitoring
│   ├── db.rs           # SQLite storage layer
│   ├── pool.rs         # string pools (columnar storage)
│   ├── effects.rs / curves.rs / oneclick.rs  # editing and enhancement
│   ├── recipes.rs                            # grouped filter recipes and pixel primitives
│   ├── assets.rs / assets_format.rs           # reading and format of the built-in parameter pack
│   ├── faces.rs / clip.rs / ai.rs            # AI capabilities (ai feature)
│   ├── device.rs / wpd.rs / win.rs           # device import and system integration
│   ├── video.rs / xmp.rs / export.rs / save.rs
│   └── ...
└── target/             # build artifacts (gitignored)
```

## Built-in Effect Parameters (Asset Pack)

Tunable values — curve presets, luma weights, the default parameters for
black-and-white / two-tone / grain, blur shape coefficients, scroll damping
constants, and so on — do not live in the source code. They are centralized in
`assets/presets.dat`, a little-endian binary container with a magic header and
version number (section names + f64 value lists). This way, adjusting the look
only requires changing data, not code.

Resolution order at startup:

1. `%APPDATA%\Maliang\presets.dat` in the user directory (if present and
   well-formed) — overridable;
2. the compile-time-embedded `assets/presets.dat` (always available, as the
   fallback).

The pack can be converted between text and binary:

```bash
# Export the currently active pack as readable text
cargo run -- --export-assets my-presets.txt

# Write an edited pack back to the user directory (takes effect on next launch)
cargo run -- --import-assets my-presets.txt
```

If the pack is corrupted, the app automatically falls back to the embedded
copy and still starts normally.

## Trademark & Disclaimer

Picasa is a trademark of Google LLC. This project is an **independent open
source implementation** with no affiliation to, and no authorization,
sponsorship, or endorsement from, Google LLC.

Maliang is an independent implementation built 独立完成, 不使用 of Picasa's source code, binaries, or asset files. Similarities in
interface and interaction design are intended to preserve a familiar
experience and to support data interoperability with Picasa (such as the
classic `picasa.ini` file format).

## Third-Party Notices

The project's own code is released under the Apache License 2.0, and the
project additionally includes or references the following third-party
content. Full attribution and license texts are collected in
[THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md); all third-party
components are used and distributed under their original license terms,
which the licensing of this project neither replaces nor overrides.

- **OpenAI CLIP** (MIT license): the BPE tokenizer implementation in
  `src/clip.rs` is written with reference to its code;
- **Cargo dependencies** (egui / eframe / wgpu / image / rusqlite / windows
  and others): all under MIT, Apache-2.0, or BSD-family licenses, compatible
  with this project's release license;
- **Models & weights**: the repository distributes no model or weight files;
  runtime resources are obtained by the user and placed into the local model
  directory, and all rights remain with their respective owners.

## License

The project's own code is released under the [Apache License 2.0](./LICENSE);
third-party components follow their respective original license terms — see
[Third-Party Notices](#third-party-notices) and
[THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md).
