# GalleryDesk - Design By Rashid — status (v1.0.0)

Maps to the "Incomplete functionality" table and milestones in the original handover.

| Item (from handover) | Status | Notes |
|---|---|---|
| Persistent folder roots | **Done** | `folders` table, per-root rescan/remove, offline detection |
| Background indexing / cancel / progress | **Done** | Off the UI thread; cancel never purges; interrupted runs resume |
| Albums, Tags | **Done** | Case-insensitive unique names; orphan tags cleaned |
| Smart albums | **Partial** | 3 built-in collections (recent, untagged, with location); no user-defined rules |
| Timeline | **Done** | Day / month / year grouping |
| Full-screen viewer | **Done** | Double-click opens an in-app slideshow: zoom, pan, rotate, favorite, details, auto-advance, bottom film-strip carousel, neighbour preloading |
| Video playback | **Done (in-app)** | Double-click plays in the in-app player via a loopback token-protected Range server. Unsupported codecs: optional playback copy made in the cache (original untouched) or open in default app |
| EXIF metadata | **Done** | Date, camera, lens, exposure, focal length, GPS. No XMP/IPTC |
| RAW pipeline | **Partial** | Embedded-JPEG preview fallback. No true RAW development; depends on file layout, tested only with a synthetic file |
| Duplicate detection | **Done (exact)** | SHA-256 of same-size files. No perceptual/similar-image matching |
| Filesystem watcher | **Done** | inotify via `notify`, 4 s debounce, setting to disable; huge trees may hit the inotify watch limit (falls back to launch/manual rescan) |
| Interactive sorting | **Done** | |
| Light theme | **Done** | Plus System mode and 5 accents |
| Functional settings | **Done** | Appearance, library, catalog backup/restore, shortcuts, about |
| Exclude folders  | **Done** | Settings → Library. Absolute paths or bare folder names; skipped by scan and watcher, existing entries purged, reversible |
| Larger UI fonts  | **Done** | Base text is now 15 px and every font size was raised by 2 px; layout constants (rows, captions, headers) adjusted |
| Rename  | **Done** | "GalleryDesk - Design By Rashid" in window title, product name, UI, README, handover. Data dir `io.gallerydesk.app`; old `io.luma.photomanager` data is migrated on first launch |
| Milestone 5: contact sheets, map view, RAW/JPEG stacks, perceptual similarity | **Not done** | GPS is shown as coordinates only |

## Defects found and fixed in the starter
1. `npm run build` failed (2 TypeScript errors).
2. `cargo` build aborted: `assetProtocol` enabled without the `protocol-asset` feature.
3. `src-tauri/icons/` missing from the zip (handover says it existed) — icons added and listed in `bundle.icon`.
4. `devUrl` used `localhost` while Vite binds `127.0.0.1` (blank window on IPv6-first systems).
5. Scan ran under a single global DB mutex on the command thread, freezing the UI.
6. Results were capped at 2,000 items — replaced by a virtualised, paged grid.
7. Thumbnail cache keyed by path only → stale thumbnails after a file changed; now keyed by path+size+mtime+rotation+size.
8. Stale rows were never removed; a missing drive would have been indistinguishable from deletion (now guarded).
9. EXIF orientation depended on the FFmpeg build; now explicit.
10. Strict EXIF parsing discarded *all* metadata when one IFD was malformed; now keeps partial results.
11. Found during testing of this pass: timeline group headers were in the opposite order to the rows (SQL `GROUP BY` ascending vs descending items) — fixed with a regression test; ffprobe gave stills a 0.04 s "duration" — fixed.
12. 0.1.0 catalogs (incompatible schema) are detected and rebuilt rather than crashing.
14. **AppImage only — all thumbnails missing:** the AppImage launcher exports its own `LD_LIBRARY_PATH`, `PATH`, `PYTHON*`, `GST_*`, … and child processes inherited them, so the host `ffmpeg`/`ffprobe` loaded the bundle's libraries and failed. All host tools (ffmpeg, ffprobe, xdg-open, dbus-send) are now started with the bundle variables stripped (`media::host_command`, tested).
15. **AppImage only — app hangs on video double-click:** the launcher pointed GStreamer at a plugin folder that is not in the bundle, so the web engine found no `appsink`/`autoaudiosink` and the WebKit web process crashed as soon as a `<video>` element was created (reproduced; log: "GStreamer element appsink not found"). The app now points GStreamer at the host's plugin directory and scanner at start-up, checks that the needed plugins exist before creating a player (otherwise it offers "Open with default app"), and keeps a crash guard: if the built-in player ever fails to report ready, the next launch switches it off automatically (toggle in Settings → Library).
16. Legacy data migration was skipped because the web engine pre-creates the new data folder; the migration now keys on the old catalog file. Settings → accent-colour click handler matched `<html data-accent>` and redrew the pane instead of the click target (made the Exclude input unusable); fixed.
17. **Previews missing when a large folder (e.g. the whole home directory) is added:** thumbnails were only rendered after the entire folder walk finished, so a slow walk meant no previews for minutes. Previews are now rendered concurrently while the walk is still running, catalog commits are smaller (40 rows) so rows appear sooner, and dependency/trash folders (`node_modules`, `venv`, `site-packages`, `__pycache__`, `snap`, `target`, `Trash`) are skipped.
18. The image/video viewer, film-strip, info panel and video background were hard-coded dark; they now follow the selected theme (light/dark).
19. **Previews still missing after the scan finished / videos opening externally (upgrade from a broken build):** rows whose preview had failed under an older build stayed marked "failed" forever (only a manual "Rescan all" retried them), and the old crash guard had persisted `video_builtin=0`. On the first start of a new version the catalog now re-queues failed previews and re-enables the built-in player (tested). Tiles whose preview cannot be rendered show the original image / the video's first frame instead of "No preview". Settings → About lists FFmpeg and built-in-player status.
20. **Video still froze the app on a real machine (and the crash guard then switched the player off on the next launch):** the AppImage carried GStreamer 1.24 core libraries but no plugins, and v0.3.1 loaded the *host's* plugins into them; a newer host GStreamer (or a GPU/VA-API driver mismatch) hangs the web process. The AppImage now bundles a complete matching plugin set (`bundleMediaFramework`: base/good/bad/libav + scanner, ~157 MB) and uses only that, with a private plugin registry; hardware (VA-API/NVDEC) decoders are ranked off so decoding is in software, and WebKit's DMABUF renderer is disabled. Verified with the host's GStreamer folder removed entirely. The host-plugin path remains only as a fallback for builds without bundled plugins.
21. **1.0.0 release:** version reset to 1.0.0 everywhere (package.json, Cargo.toml/lock, tauri.conf.json, docs); new round app icon (`public/icon.svg`, `src-tauri/icons/*`); sidebar brand reads "GalleryDesk - Design By Rashid" on one line at one font size (sidebar widened to 312 px); product name capitalised "Design By Rashid"; README rewritten for GitHub with screenshots in `docs/screenshots/`.
13. Grid froze after repeated scrolling because every library-changed event during indexing re-fetched all cached pages (up to 48 x 240 items); now only the pages around the viewport are refreshed and unchanged rows are not rebuilt.

## Verification performed
- `cargo test`: 22 integration tests against real images/video (EXIF, orientation, indexing, incremental rescan, moves, unplugged root, cancel, filters/sort/layout, rotation, duplicates, backup, legacy migration (incl. engine-created folder), AppImage environment cleaning, request merging).
- `npm run build` (tsc + vite) clean.
- Real debug build run under Xvfb/WebKitGTK with a seeded library: indexing, thumbnails via asset protocol, inspector, viewer, timeline, multi-select, favorites, album creation, duplicates, settings, both themes, first-run empty state, rotate, watcher add/delete during indexing, End-key navigation on 4,000 items.

## Not verified / caveats
- Only run on a 2-core software-rendered VM. Thumbnailing was ~8 files/s with one worker (process-spawn bound); real hardware with the default worker count will be faster, but a 100k-photo first index will still take a while.
- Real RAW/HEIC/AVIF files were not available; support depends on the installed FFmpeg.
- Video playback in the built-in player needs the host's GStreamer plugins (base + good; libav for H.265 etc.); without them the app falls back to your default player. The AppImage was tested on one host only; codec coverage on your machine is untested here.
- Keyboard shortcuts and screen-reader behaviour were implemented (ARIA roles, focus rings, reduced-motion) but not audited with assistive technology.
- Restore of a catalog takes effect at next launch.
