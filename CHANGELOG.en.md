# Changelog

All user-visible changes are documented here, newest first.
中文版见 [CHANGELOG.md](./CHANGELOG.md)；the full feature list is in
[README.md](./README.md).

## Unreleased

### Changed

- Face recognition now offers **two selectable schemes** under Options >
  General > Face recognition scheme:
  - The default is **YuNet detection + AdaFace IR-50 recognition**
    (Apache-2.0 / MIT, commercially friendly), replacing the previously used
    insightface antelopev2, which is not commercially usable;
  - **antelopev2** (SCRFD + ArcFace) remains available as an optional scheme —
    its weights are not commercially usable, bring your own at your own
    discretion.
- Embeddings remain 512-d and the library schema is unchanged; the face
  algorithm tag moves to v4 and is compared together with the scheme name,
  so switching schemes (or first launch) prompts for a face rescan. The
  detection model type is auto-detected from its output tensor count; files
  are named per scheme (default `face_det.onnx` / `face_rec.onnx`,
  antelopev2 uses `face_det_v2.onnx` / `face_rec_v2.onnx`) — see the
  "Models" section of the README for how to get them.
- The People and Semantic Search pages now show the models folder location
  and placement guidance when models are missing, and the options page shows
  per-scheme readiness, instead of failing only when a scan/index is started.

## 0.2.0 (2026-11-03)

This release focuses on three things: **searching the whole disk instantly
without importing first**, **a fully revamped gallery page**, and an
**upgraded look and experience**.

### Added

**Quick Search: find photos across the entire disk without importing first**

- The first use builds a "name index" for every file on the disk — millions
  of files in just a few seconds; afterwards, searching photos, videos, or
  documents by name returns results in milliseconds.
- Filter by type: all / images (RAW included) / videos / audio / archives /
  executables / documents.
- The top bar gains a "Search" box — type a filename to jump right in.
- Search results show thumbnails directly; you can open the file or jump to
  its containing folder.
- Administrator authorization is needed only once: once registered as a
  scheduled task, the index refreshes silently at every boot, with no
  repeated pop-ups. You can revoke or change the authorization anytime under
  Options > General.
- Before any authorization is set, an explanation dialog makes its purpose
  clear — no administrator prompt without a heads-up.

**Gallery page refresh**

- The gallery page and the import page now switch as full pages, with
  forward / back navigation.
- The left sidebar becomes a real, recursively expanded directory tree;
  scanning shows a progress percentage.
- The right sidebar gains an info drawer with four panels: properties /
  tags / people / places.
- The gallery toolbar is completed, and folder view gains album headers.
- The photo context menu now uses the system-native style.

**Effects & look**

- 6 new shader effects: hexagonal cells / block ripples / circular splatter /
  crystallize / waterfall / per-channel HSL.
- Dark / light themes.
- Built-in Alibaba PuHuiTi font for crisper Chinese text; toolbar icons
  switched entirely to an open-source icon set.
- The data directory can be moved to another drive (Options > General),
  migrating thumbnails, index, and models together.

### Improved

- Startup reconciliation: photos added or deleted while the app was closed
  enter / leave the gallery at the next launch — no waiting for a rescan;
  deleted photos are cleaned up together with their thumbnails.
- Scan candidates now go through the whole-disk index fast path, speeding up
  scans.
- The authorization explanation dialog is now a centered dialog.
- The thumbnail list no longer jitters when scrolling comes to a stop.
- After a data directory move completes, the old directory is cleaned up
  automatically to avoid two confusing copies of the data; entries that
  cannot be deleted (still in use) are kept and do not prevent the new
  directory from taking effect.
- Folder view defaults to tree display (with an expandable left
  sidebar; can be switched back to flat in the view toggles).

### Fixed

- Fixed all faces clustering into a single group due to a face embedding
  preprocessing bug (face algorithm version v3; the app will prompt for a
  rescan on first launch).
- Fixed cancelled scans discarding already-completed records: files processed
  before cancellation are now kept in the library, and a resumed scan only
  picks up the remainder without creating duplicates.
- Fixed theme styles failing entirely on light-mode systems.
- Fixed the thumbnail list turning blank mid-scroll when continuing downward
  with many photos.

## 0.1.0 (2026-10-31)

First public release: a Picasa-style local photo manager. The full feature
list is in [README.md](./README.md).

- Photo indexing & browsing: timeline / folders / albums, multi-level
  thumbnails, slideshow.
- Editing & effects: preview editing with curves / exposure / black-and-white
  / grain, plus dozens of one-click effects and style filters.
- AI (enabled by default): OCR text recognition, face clustering & naming,
  CLIP semantic search in Chinese and English.
- Color labels & star ratings with classic `picasa.ini` interoperability,
  duplicate photo detection, EXIF / GPS with KML export.
- WiFi phone import, device import, export & bake-to-save.
- Fully local, no network access.
