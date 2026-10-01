#!/usr/bin/env bash
# Builds a self-contained Ghostscript for the Linux packages (AppImage, .deb,
# .rpm). Ghostscript's own copies of jpeg/png/tiff/zlib/freetype/lcms2/
# openjpeg/jbig2dec are compiled in, and its fonts and PostScript resources
# are built into the binary, so it needs nothing from the system but libc.
# That avoids clashes like "libtiff.so.6: undefined symbol
# jpeg12_write_raw_data" when an AppImage's libraries meet the system gs.
set -euo pipefail

VER=10.08.0
TAG=gs10080
DEST="${GITHUB_WORKSPACE:-$(pwd)}/src-tauri/ghostscript"

work=$(mktemp -d)
cd "$work"
curl -fsSL -o gs.tar.gz "https://github.com/ArtifexSoftware/ghostpdl-downloads/releases/download/$TAG/ghostscript-$VER.tar.gz"
tar xzf gs.tar.gz
cd "ghostscript-$VER"

./configure \
  --without-x --disable-cups --disable-dbus --disable-gtk --disable-fontconfig \
  --without-libidn --without-libpaper --without-tesseract --without-ijs \
  --disable-contrib > configure.log
grep -iE "local|system" configure.log | head -20 || true

make -j"$(nproc)" > make.log 2>&1 || { tail -60 make.log; exit 1; }
strip bin/gs

echo "--- shared libraries used by the bundled gs:"
ldd bin/gs

mkdir -p "$DEST/bin"
cp bin/gs "$DEST/bin/gs"
cp LICENSE "$DEST/COPYING.txt" 2>/dev/null || cp doc/COPYING "$DEST/COPYING.txt"
sed 's/unmodified Windows 64-bit binaries/built from unmodified source for Linux x86_64/' \
  "${GITHUB_WORKSPACE:-$(pwd)}/.github/ghostscript-README.txt" > "$DEST/README.txt"
"$DEST/bin/gs" --version
