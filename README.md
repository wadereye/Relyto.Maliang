<div align="right">

English | [简体中文](./README.zh-CN.md)

</div>

# Relyto.Maliang (马良)

> A local photo management app that reimagines Picasa with AI.

**Why this project exists.** Google Picasa was a truly great photo
management and editing app — fast, simple, and a joy to use. Sadly, Google
discontinued it in 2016, leaving a gap that no modern tool has quite filled.
That's why this project sets out to rewrite Picasa in Rust and release it as
open source, so the experience we loved can live on.

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

The project is in early development (v0.1.0). The core browsing, indexing, and
thumbnail pipeline are already working.

## Feature Roadmap

### Implemented / In Progress
- **Photo indexing & browsing**: scans local directories and builds a SQLite
  index, with timeline and folder views.
- **Multi-level thumbnails**: a persistent background worker generates and
  caches thumbnails at four sizes — Small96 / Normal256 / Big512 /
  Preview1024 — keeping the UI smooth and responsive.
- **Timeline view**: grouped by capture date, following Picasa's view and
  sorting conventions.
- **Folder view**: watches directory changes (event debouncing + polling +
  baseline three-tier gating) and incrementally updates the index.
- **Tags / star ratings**: reads and writes the classic `picasa.ini`,
  interoperable with the original Picasa.
- **Duplicate detection**: perceptual hashing based on dHash to find visually
  similar photos (beyond the original's capabilities).
- **EXIF & GPS**: reads capture parameters and coordinates, with KML export.
- **Faces & albums**: face detection, template matching, clustering, and
  naming (enabled by default via the `ai` feature).
- **Editing & one-click enhance**: curves (natural cubic spline), exposure /
  black-and-white / grain effects; the one-click enhance set includes
  black-and-white, sepia, grain, and sharpen, plus self-designed warm tone,
  cool tone, vignette, and fade, and gamma, linear blur, focal softening,
  focal desaturation, and directional brightness / saturation / sharpness.
  Effect parameters live in an asset pack for easy tuning.
- **Style recipes**: 28 grouped filter recipes (Fade, Frame, Drop Shadow,
  Center Mosaic, Zoom Blur, Film Grain, Strong Dynamic, Thermal Imaging, Toy
  Camera, Infrared Film, Local Contrast, Vignette Glow, Edge Glow, Mat Board,
  Neon, Night Vision, Pixelate, Instant Photo, Posterize, Rounded Corners,
  Monochrome Wash, Two-Tone, Comic, Soft Glow, Pencil Sketch, Cross Process,
  Cinemascope, Sixties). Each recipe is a fixed sequence of pixel operations
  with a unified strength control that sets the blend ratio against the
  original image. A set of reusable primitive operators is also provided
  (blend modes, masks, box blur, linear blur, gradient maps, edge detection,
  zoom blur, posterization, borders, drop shadow, rotation, directional
  adjustments, and more).
- **Device import**: imports from cameras, phones, and other portable devices
  via WPD / WIA (Windows).
- **Video support**: recognizes common video formats, extracts thumbnails,
  and plays them with the system player.
- **XMP mirroring**: mirrors edits and metadata to XMP for interoperability
  with other tools.
- **Export / save**: bakes edit parameters into exports while preserving the
  original files.

### AI Capabilities (the `ai` feature is enabled by default)
- OCR text recognition
- Automatic people-grouping suggestions and semi-automatic clustering
- CLIP semantic search ("find photos with cats")

### Intentionally Out of Scope
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

Maliang is an independent implementation built from the ground up, without
using any of Picasa's source code, binaries, or asset files. Similarities in
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
