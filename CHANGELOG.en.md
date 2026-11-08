# Changelog

All user-visible changes are documented here, newest first.
中文版见 [CHANGELOG.md](./CHANGELOG.md)；the full feature list is in
[README.md](./README.md).

## Unreleased

### Changed

**Default face scheme switched to YuNet + SFace (licensing adjustment)**

- The default scheme is now **YuNet + SFace** (OpenCV Zoo): both models'
  weights are released under Apache-2.0 **with the license covering the
  weights themselves** — commercial use is OK, and the program may fetch them
  once ModelURL is configured.
- AdaFace and insightface antelopev2 become optional, non-default schemes:
  their training data (MS1MV2 / CASIA-WebFace / glint360k) is non-commercial
  research-only, and the code's MIT license does not cover it — converting to
  ONNX grants no new rights. Users bring their own weights and place them
  manually (AdaFace may be staged by a server operator at their own
  responsibility; antelopev2 is never auto-downloaded). The vendor license
  statements are labeled on the options page, in the README, and in
  THIRD_PARTY_NOTICES.md.
- The recognition pipeline adapts per scheme: SFace uses the OpenCV
  FaceRecognizerSF alignment template and raw-RGB preprocessing, outputting
  128-d vectors; clustering/suggestion thresholds switch automatically
  (SFace 0.45/0.40, AdaFace-family keeps 0.6/0.55), and template loading
  filters by dimension to prevent cross-scheme comparisons after a switch.
- Switching schemes still prompts a rescan (vectors are incompatible); the
  default change means existing AdaFace users get one rescan prompt on
  first launch.
- Suggestion matching upgraded: unnamed clusters are now matched against
  named clusters using the two highest-quality faces as probes compared
  against **all templates** of the named cluster (previously centroid vs
  centroid) — notably more robust for pose-skewed clusters.

**Model downloads work out of the box: built-in official URLs + mirror fallback**

- The program now ships with built-in official direct URLs per scheme (the two
  default-scheme SFace files point at OpenCV Zoo; the AdaFace recognition file
  is a community ONNX export of the official weights, preprocessed identically
  to the built-in path) — pressing "Download models" is enough when models are
  missing, no server configuration required.
- `ModelURL` now means **mirror fallback**: it ships pointing at a mirror
  directory and engages automatically when a built-in source answers 404;
  leave it empty to disable the fallback. Downloads still require an explicit
  user click before any traffic.
- antelopev2 weights are non-redistributable and are never fetched by the
  program; the UI now shows the official Release URL with rename instructions.

**Three anti-snowball rules for face clustering**

- Young-cluster gating: clusters with fewer than 3 members have weak centroid
  evidence and now demand a higher cosine (threshold +0.1) to absorb a face,
  so junk singleton clusters stop snowballing during incremental imports.
- Assignment margin: a face whose best and second-best cluster scores differ
  by less than 0.05 is treated as ambiguous — it is not assigned on the spot
  and waits for the next batch or a full recluster instead of polluting a
  centroid.
- Centroid-level merging: at the end of every scan batch and every full
  recluster, clusters whose centroids are close enough are merged back
  together, repairing the splits greedy clustering tends to produce
  ("splits but never merges"). User-named clusters are never auto-merged.
  Vector compatibility is unchanged — no rescan needed; recluster applies it.

**Models default to manual placement; the program no longer downloads them on its own**

- The face-recognition weights (AdaFace IR-50) were trained on MS1MV2, whose
  terms are research-only — the code's MIT license does not cover the training
  data. To leave the licensing responsibility with the user, the factory
  configuration no longer ships a built-in model directory: missing models
  show manual-placement guidance, and the first-run download prompt is gone.
- Self-hosting users can still set `ModelURL` in the `[update]` section of
  `maliang.ini` to restore downloads; confirming the redistribution license is
  the server operator's responsibility. `antelopev2` remains manual-only.
- README gained an "AdaFace weight licensing notice"; THIRD_PARTY_NOTICES.md
  documents the training-data caveat; the face-scheme option now reads
  "recommended" instead of "commercially usable".

## 0.3.0 (2026-11-07)

A major update since v0.2: **disk-wide quick find now runs as a resident
elevated service**, plus a **full-page single-photo editor**, **read-only
previews for animated images and PSD / PSB**, an **in-app self-updater**,
the **Recently Updated home album**, **double-click-to-open and context
menus for quick-find results**, and **unified configuration with automatic
model provisioning**.

### Added

**Disk-wide quick find as a resident elevated service**

- The index engine and the UI are now separate processes: volume metadata
  is only readable by an administrator, so an elevated resident service
  holds the in-memory index while the non-elevated UI queries it over a
  local named pipe; a single search returns up to 500 hits with full paths.
- If the UI cannot reach the service it falls back to per-folder scanning:
  all other features keep working, only disk-wide quick find is unavailable,
  and the UI says so explicitly.
- Subcommands for service install / uninstall / start / stop are provided,
  along with manual index rebuild and diagnostic entry points.
- The index now uses a columnar in-memory layout (a name pool plus
  fixed-width columns) and rebuilds paths by walking parent links instead of
  storing full path strings; refreshing merges new entries incrementally
  rather than re-sorting the whole index.

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

**Read-only previews for animated images and PSD / PSB**

- GIF and animated WebP now play back frame by frame with proper delays
  in the single-photo preview (read-only; files are never modified and
  baking/export are unaffected); indexing and thumbnails still use the
  first frame, with caps on frame count and memory use.
- PSD / PSB files render their embedded composite image for thumbnails and
  previews; when no composite is present, layers are approximated instead.
  Read-only files never enter the editing or export pipelines.

**In-app self-updater**

- "Help > Check for Updates" on demand, plus optional idle-time automatic
  checks driven by configuration.
- Downloads over HTTPS in ranged chunks with streaming SHA-256
  verification and resume support; after verification an elevated helper
  performs the install (one UAC prompt stops the service, swaps the
  executable and restarts it); the new version takes effect on next
  launch, and failures roll back to the previous executable.
- Server URL, check interval, proxy, chunk size and the model directory are
  all configured in one `maliang.ini` (older data-location settings and
  `update.ini` are migrated automatically on first launch; the server
  manifest format ships as an example file).
- The official manifest URL and models directory are built in as factory
  defaults: a fresh install, or wiping the local configuration, no longer
  disables update checks or model downloads. Set your own addresses in
  `maliang.ini` to switch servers; an empty value disables that capability.
- When auto-download is off, a new version is only announced and the
  user decides when to update.
- `--version` prints the version for scripts; "check only" and
  "download but do not install" entry points are available for diagnosis.

**Recently Updated home album**

- The home view is now a virtual album of photos that already have
  on-disk thumbnails, ordered by update time with a member cap that
  evicts the oldest; the album header offers slideshow and manual
  refresh.

**Single instance and unified configuration**

- Only one UI process per login session; launching a second one activates
  the existing window and shows a hint, instead of two windows competing
  for the index service.
- Data directory, updater settings and model source now live in a single
  `maliang.ini`; the two older configuration files are migrated at startup.

**Model readiness and background download**

- The People and Semantic Search pages, plus the options page, show the
  models folder and placement guidance when models are missing.
- Once a server-side model directory is configured, "Download models"
  fetches them in a background thread with ranged resume; each file is
  really loaded once through the runtime before it is accepted, and files
  that fail to load are discarded.
- Setting `ModelURL` to an empty value restores fully manual placement;
  the built-in address is the official distribution directory, and no
  third-party download links are shipped.

**Acting on quick-find results**

- Double-click a result row to open it: folders open in Explorer; images
  open the in-app preview/editor, with a fallback to the system's
  associated app (and an explanation) for files not yet in the library;
  videos and everything else open with the associated app.
- The result-row context menu matches the gallery menu; files outside the
  library support open / open containing folder / copy full path /
  system context menu.
- The sidebar "Folders" group gains common places: Documents / Desktop /
  Pictures / Videos.

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
  algorithm tag moves to v5 and is compared together with the scheme name,
  so switching schemes (or first launch) prompts for a face rescan. The
  detection model type is auto-detected from its output tensor count; files
  are named per scheme (default `face_det.onnx` / `face_rec.onnx`,
  antelopev2 uses `face_det_v2.onnx` / `face_rec_v2.onnx`) — see the
  "Models" section of the README for how to get them.
- The People and Semantic Search pages now show the models folder location
  and placement guidance when models are missing, and the options page shows
  per-scheme readiness, instead of failing only when a scan/index is started.
- Directory watching is now an in-house implementation: the previous
  third-party backend could terminate the whole process when a file inside
  a watched folder was deleted on Windows. Rename events are now paired at
  this layer so stars, labels and other user metadata survive.
- Updater TLS now uses rustls: the Windows system TLS stack had its
  handshake reset by the target server, so an independent implementation
  is used for stable downloads.

### Improved

- Corrected the volume change-journal reason flags, fixing newly created
  and genuinely deleted files being missed by the service's incremental
  updates; incremental finalization is now incremental too, removing
  quick-find queue jitter.
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
  view instead of re-sorting the whole index.
- When a photo moves to the recycle bin, stale index rows are removed by
  file identifier so search no longer lists deleted files.
- Folder index states are now explicit: deliberate import / ask no more /
  never index are recorded in layers, and mistakenly excluded folders can
  be purged from the library in one click.
- Face scanning upgrade: images are uprighted by EXIF orientation before
  detection (rotated phone photos no longer miss faces), detection input
  grows 640→1280 for better small-face recall, and the cluster assignment
  threshold moves 0.5→0.6 so different people merge less eagerly.

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
- Fix the process exiting when a file is deleted inside a watched folder,
  and user metadata being lost on rename.
- Fix incremental updates missing creations and deletions, and cloud
  placeholder files triggering online recalls during backfill.
- Fix reading an empty configuration before migration completes, which
  left the updater without its server URL.
- Fix photos with no detected faces staying at the head of the scan queue
  forever and starving everything behind them (the "analyzed" mark is now
  decoupled from whether faces were found).

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
