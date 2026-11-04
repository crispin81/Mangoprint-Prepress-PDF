#!/usr/bin/env bash
# Puts the Wayland launch setting in an AppImage's GTK hook and packs it again:
#   scripts/patch-appimage-hook.sh path/to/App.AppImage
# The Tauri CLI writes its own linuxdeploy-plugin-gtk.sh over ~/.cache/tauri's on every bundle
# (locally and in GitHub Actions), so the patched copy in .github/appimage/ is lost and the
# AppImage ships with GDK_BACKEND commented out. The hook has to be changed after bundling.
# Run it on every Linux AppImage before it's installed or released; it stops if the packed
# file can't be confirmed.
set -euo pipefail
image="$(readlink -f "$1")"
chmod +x "$image"
tool="${LINUXDEPLOY_APPIMAGE_PLUGIN:-$HOME/.cache/tauri/linuxdeploy-plugin-appimage.AppImage}"
[ -x "$tool" ] || { echo "Needs $tool (made by a local 'npm run tauri build')" >&2; exit 1; }
wayland='export GDK_BACKEND=wayland,x11 # Prefer native Wayland (correct fractional scaling), fall back to X11 automatically if unavailable'

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
(cd "$work" && "$image" --appimage-extract >/dev/null)
app="$work/squashfs-root"
hook="$app/apprun-hooks/linuxdeploy-plugin-gtk.sh"
if ! grep -qxF "$wayland" "$hook"; then
  grep -q '^# export GDK_BACKEND=' "$hook" || { echo "Hook has no GDK_BACKEND line to replace" >&2; exit 1; }
  sed -i "s~^# export GDK_BACKEND=.*~${wayland}~" "$hook"
fi

# Pack again (same AppDir, new hook) next to the original, then swap it in.
out="$work/out.AppImage"
mv "$app" "$work/App.AppDir"
(cd "$work" && ARCH=x86_64 LDAI_OUTPUT="$out" "$tool" --appimage-extract-and-run --appdir App.AppDir >/dev/null)

# What matters is what's in the packed file, so read it back from there.
check="$work/check"
mkdir "$check"
(cd "$check" && "$out" --appimage-extract 'apprun-hooks/*' >/dev/null)
grep -qxF "$wayland" "$check/squashfs-root/apprun-hooks/linuxdeploy-plugin-gtk.sh" || { echo "Packed AppImage has no Wayland line" >&2; exit 1; }
chmod +x "$out"
mv -f "$out" "$image"
echo "AppImage hook confirmed (native Wayland, X11 fallback): $image"
