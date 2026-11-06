# Changelog

All user-visible changes are documented here, newest first.
中文版见 [CHANGELOG.md](./CHANGELOG.md)；the full feature list is in
[README.md](./README.md).

## 0.3.0 (2026-11-06)

This release focuses on four things: a **full-page single-photo editor**,
**animated-image and PSD previews**, an **in-app self-updater**, and the
**Recently Updated home album**.

### Added

**Full-page single-photo editor**

- Double-click a photo to enter a full-page editing view modeled after
  classic photo managers: top bar with back-to-library, previous/next,
  slideshow, compare and zoom; left tool panel with category pages
  (Basic Fixes / Tuning / Effects / Curves / Crop / Info, each tool shown
  as an icon with a label) where tools apply on click instead of hiding
  in menus; persistent undo / redo; the panel bottom hosts an RGB
  histogram plus camera info and EXIF details.
- A persistent properties panel on the right with four tabs
  (Properties / Captions / Tags / Effects) showing file name, location,
  size, dimensions, capture date and EXIF details.
- A bottom filmstrip of neighboring photos (4 on each side); click a
  thumbnail to jump to that photo.
- A / B / AA comparison modes: view the edited result, the original, or
  both side by side.
- Tools with a strength slider (Fill Light / Sharpen / Vignette / Grain
  and friends) preview live; "Apply" commits, "Cancel" discards, and
  every step is undoable.

**Animated images and PSD previews**

- GIF and animated WebP now play back frame by frame with proper delays
  in the single-photo preview (read-only; files are never modified and
  baking/export are unaffected); indexing and thumbnails still use the
  first frame.
- PSD files now render their embedded composite image for thumbnails and
  previews instead of a placeholder icon.

**In-app self-updater**

- "Help > Check for Updates" on demand, plus optional idle-time automatic
  checks driven by configuration.
- Downloads over HTTPS in ranged chunks with streaming SHA-256
  verification and resume support; after verification an elevated helper
  performs the install (one UAC prompt stops the service, swaps the
  executable and restarts it); the new version takes effect on next
  launch, and failures roll back to the previous executable.
- Update server URL, check interval, proxy and chunk size are all
  configurable via `update.ini` (a commented template is generated on
  first launch; the server manifest format ships as an example file).
- When auto-download is off, a new version is only announced and the
  user decides when to update.

**Recently Updated home album**

- The home view is now a virtual album of photos that already have
  on-disk thumbnails, ordered by update time with a member cap that
  evicts the oldest; the album header offers slideshow and manual
  refresh.

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

### Improved

- The first-run "quick find service" and "photo scope" prompts are now a
  single two-step wizard window instead of two stacked dialogs.
- A deletion reconciliation pass runs at startup: photos deleted externally
  while the app was closed leave the library (and their thumbnails are
  purged); offline roots are skipped entirely to avoid false removals.
- Thumbnail cache health loop: a background startup pass removes orphan
  cache files with no library row; tiers whose ledger says "generated" but
  whose file went missing (cache folder wiped or moved externally) are
  unlisted and rebuilt automatically; a single corrupt cache file is
  deleted and regenerated on load.
- The disk-wide name index now skips the app's own output folders, the
  recycle bin and system folders: such files no longer enter the index,
  dominate the Recently Updated wall, or generate self-inflicted watch
  events.
- Index refreshing now merges new entries incrementally into the ordered
  view instead of re-sorting everything.
- When a photo moves to the recycle bin, stale index rows are removed by
  file identifier so search no longer lists deleted files.
- Folder index states are now explicit: deliberate import / ask no more /
  never index are recorded in layers, and mistakenly excluded folders can
  be purged from the library in one click.

### Fixed

- Fix uncommitted edits bleeding into the next photo when switching
  inside the editor.
- Fix style filters and one-click effects whose strength slider did not
  affect the preview until "Apply" was pressed.
- Fix the "show original" mode displaying the edited image after zooming
  in.
- Fix repeated full-image decode retries causing stalls after a failed
  zoom decode.
- Fix stale edits lingering in the preview after undo / redo / clear
  effects.

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
