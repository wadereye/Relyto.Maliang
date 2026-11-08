| English | [简体中文](./README.zh-CN.md) |

# Relyto.Maliang (神笔马良 / Maliang)

> Your local photo AI butler
> Instant preview · Smart tags · Chat-to-search · One-click done

**Official website & download**: <https://www.nicexm.com/maliang/>

**Why this project exists.** Google Picasa was a truly great photo management and editing app — fast, simple, and a joy to use. Sadly, Google discontinued it in 2016; many photo tools have appeared since, but I never found one I truly liked. That's why Maliang (神笔马良) was born: written independently in Rust and released as open source, to carry that classic experience forward and add the capabilities the old tool never had.

Maliang (神笔马良) is a desktop-first local photo management tool that aims to modernize the classic Picasa 3 experience: smoother browsing, smarter organization, and native, offline, privacy-first AI capabilities. All of your data stays on your own machine — nothing is uploaded to the cloud, and your private photos are never scanned and sent anywhere.

If Maliang is useful to you, please consider giving the repo a [⭐ Star](https://github.com/wadereye/Relyto.Maliang/stargazers);

If something is missing or broken, feel free to open an [Issue](https://github.com/wadereye/Relyto.Maliang/issues).

For feature requests or to join the community group, you can email me, or scan my WeChat Official Account QR code at the bottom of this page, then tap the "AI 交流" (AI Chat) menu — or send the message "加群" (join group) — and I'll add you to the group. Let's build and improve it together.

## [Changelog](./CHANGELOG.en.md)

## Feature Overview (implemented)

### Browsing & Organizing

- **Full-disk scan & index**: once a folder is added to the library, a local index (SQLite) is built automatically; folders come first — your originals are never copied or moved.
- **Multiple views**: timeline (grouped by capture date) / folders / albums (named collections that never touch files).
- **Left sidebar directory tree**: a real, recursively expanded tree with a scan progress percentage.
- **Multi-level thumbnails**: generated and cached on demand at four sizes — 96 / 256 / 512 / 1024 — keeping browsing smooth.
- **Automatic updates**: while running, changes are tracked live via file system events; files added or removed while the app was closed are reconciled automatically at the next launch.
- **Slideshow**: play the selected photos or the entire current view.
- **Sorting & thumbnail size**: by capture date / filename, ascending or descending, normal / small.
- **Multi-format support**: common image formats including JPG, PNG, WebP, GIF, PSD and more.

### Quick Search

- **Whole-disk filename index**: the first use builds a name index for the entire disk (a few seconds for millions of files); afterwards, searching for photos, videos, or documents returns results in milliseconds.
- **Filter by type**: all / images (RAW included) / videos / audio / archives / executables / documents.
- **Global file search box**: type a filename anywhere to jump into quick search; results return in seconds even across millions of files.
- **One-time authorization, silent background updates**: once registered as a scheduled task with administrator authorization, the index stays fresh automatically at boot; it can be revoked anytime under Options > General.
- **Multiple quick-index fallbacks**: on Windows, NTFS drives use a fast scan; when NTFS is unavailable, an adjusted `walkdir` traversal serves as the fallback.

### Editing & Effects

- **Preview editing**: rotate, crop, zoom & pan, curves, exposure, contrast, color temperature, tint, sharpen, blur, grain, vignette, and more — all non-destructive; changes are only baked to disk via F2 save.
- **One-click effects**: auto contrast / auto color / warm tone / cool tone / positive film / LOMO / backlight compensation and dozens more, aligned with the original Picasa's behavior.
- **Style filters**: dozens of grouped recipes — film grain, toy camera, instant photo, cinemascope, sixties, pencil sketch, cross process, two-tone, neon, night vision, mat board, rounded corners…
- **Shader effects**: hexagonal cells, block ripples, circular splatter, crystallize, waterfall, per-channel HSL.

### AI Capabilities (the `ai` feature is enabled by default, can be trimmed off)

- **OCR text recognition**: text inside photos becomes searchable (not fully complete as of v0.3).
- **Face organizing**: detection, clustering, quality scoring, semi-automatic naming suggestions, ignored clusters (multiple schemes implemented; the user chooses which model to use).
- **CLIP semantic search**: bilingual Chinese/English descriptive search such as "find photos with cats" (not fully complete as of v0.3).

### Import & Export

- **WiFi phone import**: visit the computer's address from a phone browser and send photos into Incoming.
- **Device import**: import from USB-connected cameras, phones, and other portable devices via WPD / WIA (Windows).
- **Export & baking**: export by the current view range with edit parameters baked into the output; the original files are never touched.
- **Video**: recognizes common formats, extracts thumbnails, and plays them with the system player.

### Management & Interoperability

- **Duplicate photo detection**: perceptual hashing finds visually similar photos (beyond the original's capabilities).
- **Color labels / star ratings**: reads and writes the classic `picasa.ini`, interoperable with the original Picasa.
- **XMP mirroring**: edits and metadata are mirrored to XMP for interoperability with other tools.
- **EXIF / GPS**: reads capture parameters and coordinates, with KML geotag export.
- **Folder manager**: add/remove watched directories, rescan each one, and clean up missing files.
- **Index backup & compaction**; **relocatable data directory** (thumbnails / index / models all move together).

### Interface & Experience

- Dark / light themes; built-in Alibaba PuHuiTi Chinese font; icons use the open-source Lucide set.
- Classic five-menu layout (File / Edit / View / Tools / Help); native right-click menus on photos.
- Right info drawer: properties / tags / people / places.
- **Fully local: no network access, no telemetry.**

### Intentionally Not Planned (present in the original Picasa, but not planned for now)

Advanced retouching workflows (retouch / picnik), collage / poster / CD burning, online sharing (Picasa Web / social platforms / email / printing), TWAIN camera scanning, cloud album sync, and Picasa2 migration.

## Preview-only formats

GIF, PSD and PSB are indexed and shown as read-only quick previews. They are never re-encoded; batch export copies the original bytes unchanged.

- **GIF**: first frame only. Animation playback, per-frame editing and GIF encoding are not provided.
- **PSD / PSB**: the merged composite image already stored in the file, parsed in-house from Adobe's public *Photoshop File Formats Specification*. Layers, blend modes, smart objects, CMYK / Lab and 16/32-bit documents are not rendered. A file saved without a merged composite has no preview on systems without a system thumbnail provider.
- **WebP**: lossy and lossless decoding; animated WebP also shows its first frame. Export writes lossless WebP only.

No Adobe code is bundled; see [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md).

## Roadmap

- Prioritize a stable Windows build, with instant photo search and instant preview across many image formats.
- Improve launch stability and the stability of photo viewing/preview; achieve instant search and instant viewing across one hundred thousand photos.
- Improve simple photo effect processing.
- Implement photo deduplication, cleanup, and similar needs.
- Implement multi-channel photo backup, sync, and similar needs.
- Implement local face recognition, improve accuracy, and make finding face photos fast.
- Auto-tag photos to enable text-based image search (e.g., "help me find where my ID card is stored").
- Support multiple operating systems, e.g., a usable Linux version.
- Introduce natural-language conversational image processing.
- Provide multi-scenario presets for image processing.

## Tech Stack

- **Language**: Rust 2021
- **UI**: [egui](https://github.com/emilk/egui) / [eframe](https://github.com/emilk/egui) 0.33 (wgpu rendering)
- **Database**: [rusqlite](https://github.com/rusqlite/rusqlite) (bundled SQLite)
- **System integration**: the `windows` crate (WPD / WIA / COM on Windows)
- **Imaging**: `image` 0.25 (JPEG / PNG / WebP / TIFF / BMP / GIF); PSD / PSB read-only composite preview implemented in-house (see [Preview-only formats](#preview-only-formats))

## Building

### Requirements

- Windows 10/11 (the project currently targets Windows only, relying on the `windows` crate for device import)
- The [Rust toolchain](https://www.rust-lang.org/tools/install) (stable; a recent version is recommended)
- Network access is required during the build to fetch dependencies from crates.io

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

The `ai` feature pulls in ONNX Runtime and model files (OCR / faces / CLIP) and is **enabled by default**:

```bash
cargo build --release          # default = includes ai
```

If you don't want the extra size and dependencies:

```bash
cargo build --release --no-default-features
```

In a slim build, the "People" and "Semantic Search" views will indicate that they were not compiled in; everything else works as usual. Before enabling the feature, make sure the `onnxruntime` runtime and the corresponding model weights are in place.

### Models (not bundled, place them yourself)

Model files and weights are not distributed with the program (size and licensing reasons). At runtime they live in the `models\` subfolder of the **data directory** (default `%APPDATA%\Relyto\Maliang\models\`; the data directory can be relocated under Options > General). When a model is missing, the corresponding feature shows placement guidance instead of failing; other features are unaffected.

The manifest URL is built into the program as a factory default, so a fresh install or a wiped local configuration keeps update checks working. Face-model downloads work **out of the box** — the program ships with built-in official direct URLs per scheme (the two files of the **default scheme (SFace)** point at OpenCV Zoo, Apache-2.0 covering the weights themselves, commercial use OK; the AdaFace recognition file is a community ONNX export of the official weights, whose training-data licensing chain needs to be reviewed by the user, see the "Non-default scheme weight licensing" below), and `ModelURL` ships pointing at a mirror directory that acts as a fallback whenever a built-in source lacks a file; the `antelopev2` scheme's weights may not be redistributed, so that scheme stays manual-placement only in every case. In the `[update]` section of the unified `maliang.ini`: `UpdateURL` (manifest URL) falls back to the built-in address when omitted, points at your own server to switch sources, and disables auto-update when set to an empty value; `ModelURL=https://your.host/maliang/models/` (a mirror models folder, trailing `/`) serves as a fallback source besides the built-in official URLs: at startup the program checks the models folder and, if something is missing, asks — the download only starts once you confirm in that dialog or press 「下载模型」 (Download models) under Options > General (the recognition model is large, so traffic is never spent on the user's behalf by default). Downloading runs in the background in Range segments with progress in the status bar and the settings page; an interrupted transfer keeps its breakpoint and the next press resumes it, and when one source answers 404 the next candidate source is tried automatically. Before a file is put in place it is actually loaded through ONNX Runtime, and anything that fails to load is discarded. Leaving `ModelURL` empty or blank disables the mirror fallback; the `antelopev2` scheme's weights may not be redistributed, so that scheme stays manual-only in every case. If you operate a server and distribute models from it, confirm yourself that the redistribution complies with the relevant licenses. The copy next to `maliang.exe` wins if present (portable layout), otherwise `%APPDATA%\Relyto\Maliang\maliang.ini`; the "打开配置文件" (Open config file) button, present both under Help > Check for updates and in the face-scheme block of Options > General, opens it directly.

| File | Purpose | Source & license |
|---|---|---|
| `face_det.onnx` | Face detection (YuNet, 2023mar release) | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo/blob/main/models/face_detection_yunet/), Apache-2.0 |
| `face_rec_sface.onnx` | Face recognition, 128-d (**default scheme** SFace) | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo/blob/main/models/face_recognition_sface/), Apache-2.0 (**weights included**, commercial use OK) |
| `face_rec.onnx` | Optional: face recognition, 512-d (AdaFace IR-50 exported to ONNX) | [mk-minchul/AdaFace](https://github.com/mk-minchul/AdaFace), code under MIT; weights trained on MS1MV2 (non-commercial), see the notice below |
| `face_det_v2.onnx` / `face_rec_v2.onnx` | Optional: insightface antelopev2 (SCRFD + ArcFace, **weights are not commercially usable**, bring your own) | [deepinsight/insightface](https://github.com/deepinsight/insightface), per the official weight license |
| `ch_PP-OCRv4_det_infer.onnx` / `ch_PP-OCRv4_rec_infer.onnx` / `ppocr_keys_v1.txt` | Chinese OCR | PP-OCRv4 (PaddleOCR, Apache-2.0) |
| `clip_vision.onnx` / `clip_text.onnx` / `clip_merges.txt` | Semantic search | OpenAI CLIP (MIT), exported to ONNX |

The program fetches by fixed file names only (`<ModelURL>face_det.onnx` + `<ModelURL>face_rec_sface.onnx` for the default scheme, see the table for the others), so placing files with those names on your own server is enough to switch sources. The official URLs below are for lining things up when you stage the files, and for manual placement: prefer the official links, and fall back to an accelerator mirror in China only when the official address is unreachable.

#### Where each face model can actually be downloaded from (measured one by one, 2026-11-07)

| File name in the program | Official URL | Accelerator mirror reachable in China | Measured result |
|---|---|---|---|
| `face_det.onnx` | <https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx> | OpenCV's official HuggingFace repo <https://huggingface.co/opencv/face_detection_yunet>, mirrored at <https://hf-mirror.com/opencv/face_detection_yunet/resolve/main/face_detection_yunet_2023mar.onnx> | Official URL: 200 / 232,589 bytes; `hf-mirror.com` gives the same 200 / 232,589 (`huggingface.co` itself depends on your network — it was unreachable during this measurement). ⚠️ The same path on `raw.githubusercontent.com` returns only a 131-byte Git-LFS pointer — you must go through `media.githubusercontent.com`. opencv_zoo's GitHub Releases carry no model assets. The GitCode mirror of opencv_zoo **cannot serve as a direct source**: `.../raw/main/...onnx` answers 200 with a 6 KB HTML shell, `file.gitcode.com` answers 401, and the `raw.gitcode.com` path is 404. |
| `face_rec_sface.onnx` | <https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_recognition_sface/face_recognition_sface_2021dec.onnx> | OpenCV's official HuggingFace repo <https://huggingface.co/opencv/face_recognition_sface>, mirrored at <https://hf-mirror.com/opencv/face_recognition_sface/resolve/main/face_recognition_sface_2021dec.onnx> | Same repo, same rules as YuNet: fetch via `media.githubusercontent.com` or HuggingFace — `raw.githubusercontent.com` only gives an LFS pointer. Official URL: 200 / 38,696,353 bytes (≈36.9 MiB, measured 2026-11-08). Apache-2.0 **covers the weights**, so the program may auto-download it once ModelURL is configured. |
| `face_rec.onnx` | AdaFace publishes PyTorch weights only, hosted on Google Drive (IR-50 / MS1MV2: <https://drive.google.com/file/d/1eUaSHG4pGlIZK7hBkqjyp2fc2epKoBvI>) | No official mirror | The official repo has no ONNX asset at all (GitHub Releases are empty), and the author's later CVLFace repo on HuggingFace ships only `model.safetensors`. So this file is either converted with the script below or staged on your server once converted; AdaFace ONNX files circulating elsewhere are not official releases. ⚠️ These weights were trained on MS1MV2 (a research-only dataset) and their licensing chain is unresolved — read the AdaFace weight licensing notice before commercial use. |
| `face_det_v2.onnx`, `face_rec_v2.onnx` | <https://github.com/deepinsight/insightface/releases/download/v0.7/antelopev2.zip> (a release asset under tag `v0.7`; `buffalo_l.zip` sits on the same page) | ModelScope's `deepghs/insightface` is an automated community mirror, not official | Official package is 360,662,982 bytes (≈344 MiB; size measured through the GitHub API). After unpacking, rename the detector (SCRFD 10G, `scrfd_10g_bnkps.onnx` inside the package) to `face_det_v2.onnx` and the recognizer (ArcFace R100, `glintr100.onnx`, which is where the 512-d side comes from) to `face_rec_v2.onnx` — take the names actually present in the package as authoritative. The `det_10g.onnx` / `w600k_r50.onnx` files inside `buffalo_l.zip` belong to a different scheme and are not what these names mean. **Redistribution is not permitted by the weight license, so the program never downloads this scheme.** |

All three face schemes are supported; pick one under **Options > General > Face recognition scheme**. The default is **YuNet + SFace** (both models Apache-2.0 with weights included — commercial use OK, and auto-downloadable via ModelURL). Vectors across schemes are incompatible — switching prompts a rescan, and clustering/suggestion thresholds switch with the scheme automatically. The detection model type is auto-detected from its output tensor count, so just name the files per the chosen scheme.

#### Non-default scheme weight licensing (read this before choosing AdaFace / antelopev2)

- **YuNet / SFace (default scheme, listed for reference)**: both models come from OpenCV Zoo and are released under Apache-2.0 **with the license covering the weight files themselves** — commercial use and redistribution are permitted (keep the license and attribution notices, see THIRD_PARTY_NOTICES.md).
- **AdaFace (optional, bring your own weights)**: the code is MIT (© 2022 Minchul Kim), but the official model card explicitly asks users to *follow the license of the training dataset*; the pretrained weights were trained on MS1MV2 (the cleaned MS-Celeb-1M) and CASIA-WebFace, whose terms are **non-commercial, research-only**. The code's MIT license does **not** cover the training data, and converting the weights to ONNX (or any other format) grants no new rights. Choosing this scheme means you supply the weights and carry the licensing review yourself.
- **antelopev2 (optional, bring your own weights)**: insightface's official disclaimer for its published models reads *The pretrained models we provided with this library are available for non-commercial research purposes only*. These weights **may not be redistributed**; the program never downloads them and offers manual placement only.

This project therefore handles the non-default schemes as follows:

- **Manual download is the recommended path**: obtain the weights yourself via the links below, convert them (AdaFace), and place the ONNX into the models folder — the licensing responsibility rests with the user.
- If you run your own server and choose to redistribute the AdaFace weights, put its address in `ModelURL` — the redistribution and its licensing consequences are the server operator's responsibility, not this program's; antelopev2 never enters the download pipeline.
- For stricter licensing needs, watch for weights re-released upstream under clearer terms, or use a model you trained yourself (AdaFace's network and conversion script are MIT-licensed too).

Face model setup:

- **Detection (YuNet, shared by the default and AdaFace schemes)**: download the `face_detection_yunet_2023mar.onnx` linked above, rename it to `face_det.onnx`, and drop it into the models folder. Fetch it through `media.githubusercontent.com` or HuggingFace — the same path on `raw.githubusercontent.com` gives you a 131-byte Git-LFS pointer instead of the model, which the load check rejects.
- **Recognition (SFace, default scheme)**: download `face_recognition_sface_2021dec.onnx` (Apache-2.0, weights included), rename it to `face_rec_sface.onnx`, and place it in the models folder (128-d output). With `ModelURL` configured the program can fetch these two files for you.
- **Recognition (AdaFace, optional)**: the official repository ships PyTorch weights, which must be converted to ONNX first. Download the IR-50 weights per its README (e.g. `adaface_ir50_ms1mv2.ckpt`) and run the following from the AdaFace repository root:

  ```python
  # pip install torch onnx
  import torch
  from net import build_model

  model = build_model("ir_50")
  ckpt = torch.load("adaface_ir50_ms1mv2.ckpt", map_location="cpu")
  state = ckpt.get("state_dict", ckpt)
  model.load_state_dict({k.replace("module.", ""): v for k, v in state.items()})
  model.eval()
  torch.onnx.export(
      model, torch.randn(1, 3, 112, 112), "face_rec.onnx",
      input_names=["input"], output_names=["embedding"], opset_version=17,
  )
  ```

  Put the resulting `face_rec.onnx` into the models folder (512-d output, matching the vector dimension stored in the library).
- **Optional scheme (antelopev2)**: unpack the official `antelopev2.zip` and rename `scrfd_10g_bnkps.onnx` to `face_det_v2.onnx`, `glintr100.onnx` to `face_rec_v2.onnx` (the 512-d side is the latter) before placing them. `det_10g.onnx` / `w600k_r50.onnx` come from the buffalo_l scheme and are not the files this scheme expects — do not rename those instead. The weight license forbids redistribution, so this scheme stays manual: the program never downloads it.

Model files remain the property of their respective owners; see [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md) for attribution and license texts.

### Common Commands

```bash
cargo check          # type checking only (fast)
cargo test           # run tests (includes AI module tests by default)
cargo test --no-default-features   # tests for the slim build
```

### Build Troubleshooting (Windows)

- The project depends on the `windows` crate and COM, so use the native MSVC toolchain provided by the official Rust installer.
- If the C drive is short on space and `cargo`'s temporary files cause errors, point the temp directory elsewhere:
  ```powershell
  $env:TMP = "D:\\build-tmp"; $env:TEMP = "D:\\build-tmp"
  cargo build --release
  ```
- If resource compilation fails with "Can not find Windows SDK", put the full path of `rc.exe` into `RC_PATH` and retry (with the Windows SDK installed it usually lives under `C:\Program Files (x86)\Windows Kits\10\bin\<version>\x64\`):
  ```powershell
  $env:RC_PATH = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\rc.exe"
  cargo build --release
  ```
- The first build compiles a large number of dependencies and takes a while — please be patient.

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

Tunable values — curve presets, luma weights, the default parameters for black-and-white / two-tone / grain, blur shape coefficients, scroll damping constants, and so on — do not live in the source code. They are centralized in `assets/presets.dat`, a little-endian binary container with a magic header and version number (section names + f64 value lists). This way, adjusting the look only requires changing data, not code.

Resolution order at startup:

1. `%APPDATA%\Maliang\presets.dat` in the user directory (if present and well-formed) — overridable;
2. the compile-time-embedded `assets/presets.dat` (always available, as the fallback).

The pack can be converted between text and binary:

```bash
# Export the currently active pack as readable text
cargo run -- --export-assets my-presets.txt

# Write an edited pack back to the user directory (takes effect on next launch)
cargo run -- --import-assets my-presets.txt
```

If the pack is corrupted, the app automatically falls back to the embedded copy and still starts normally.

## Trademark & Disclaimer

Picasa is a trademark of Google LLC. This project is an **independent open source implementation** with no affiliation to, and no authorization, sponsorship, or endorsement from, Google LLC.

Maliang is an independently written implementation in Rust; this repository contains and distributes no Google source code, binaries, or asset files. Similarities in interface and interaction design are intended to preserve a familiar experience and to support data interoperability with Picasa (such as the classic `picasa.ini` file format).

Adobe and Photoshop are trademarks or registered trademarks of Adobe Inc. Mentioning PSD / PSB here describes file-format compatibility only and implies no affiliation, authorization, sponsorship, or endorsement, and makes no claim of Adobe compatibility certification.

## Third-Party Notices

The project's own code is released under the Apache License 2.0, and the project additionally includes or references the following third-party content. Full attribution and license texts are collected in [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md); all third-party components are used and distributed under their original license terms, which the licensing of this project neither replaces nor overrides.

- **OpenAI CLIP** (MIT license): the BPE tokenizer implementation in `src/clip.rs` is written with reference to its code;
- **Cargo dependencies** (egui / eframe / wgpu / image / rusqlite / windows and others): all under MIT, Apache-2.0, or BSD-family licenses, compatible with this project's release license;
- **Models & weights**: the repository distributes no model or weight files; runtime resources are obtained by the user and placed into the local model directory, and all rights remain with their respective owners.

## License

The project's own code is released under the [Apache License 2.0](./LICENSE); third-party components follow their respective original license terms — see [Third-Party Notices](#third-party-notices) and [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md).

## WeChat Official Account QR Code

Scan the code, then tap "AI 交流" (AI Chat) to add my WeChat, and I'll pull you into the group.

![WeChat Official Account QR code](assets/images/contact_gzh.jpg)
