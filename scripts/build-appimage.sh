#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
command -v ffmpeg >/dev/null || { echo "Warning: ffmpeg not found; media previews will be unavailable."; }
# The AppImage bundles GStreamer plugins (bundleMediaFramework); they are copied from this machine.
for f in /usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstapp.so /usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstlibav.so; do
  [ -f "$f" ] || echo "Warning: $f missing - install gstreamer1.0-plugins-base/-good/-bad/-libav/-gl before building."
done
npm install
npm run tauri build -- --bundles appimage
echo
echo "Build complete. AppImage output:"
find src-tauri/target/release/bundle/appimage -maxdepth 1 -type f -name '*.AppImage' -print
