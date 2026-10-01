#!/usr/bin/env bash
# Builds a self-contained universal (Apple Silicon + Intel) Ghostscript for
# the macOS app bundle. Like the Linux build, Ghostscript's own image/font
# libraries and resources are compiled in, so it only needs macOS's system
# libraries. Built natively for arm64, and for x86_64 under Rosetta, then
# joined with lipo.
set -euo pipefail

VER=10.08.0
TAG=gs10080
DEST="${GITHUB_WORKSPACE:-$(pwd)}/src-tauri/ghostscript"
CONF=(--without-x --disable-cups --disable-dbus --disable-gtk --disable-fontconfig
      --without-libidn --without-libpaper --without-tesseract --without-ijs --disable-contrib)

softwareupdate --install-rosetta --agree-to-license >/dev/null 2>&1 || true

work=$(mktemp -d)
cd "$work"
curl -fsSL -o gs.tar.gz "https://github.com/ArtifexSoftware/ghostpdl-downloads/releases/download/$TAG/ghostscript-$VER.tar.gz"

build() { # arch
  rm -rf "src-$1" && mkdir "src-$1" && tar xzf gs.tar.gz -C "src-$1" --strip-components=1
  pushd "src-$1" >/dev/null
  if [ "$1" = x86_64 ]; then
    arch -x86_64 env CC="clang -arch x86_64" ./configure "${CONF[@]}" > configure.log
    arch -x86_64 make -j"$(sysctl -n hw.ncpu)" > make.log 2>&1 || { tail -60 make.log; exit 1; }
  else
    env CC="clang -arch arm64" ./configure "${CONF[@]}" > configure.log
    make -j"$(sysctl -n hw.ncpu)" > make.log 2>&1 || { tail -60 make.log; exit 1; }
  fi
  strip bin/gs
  popd >/dev/null
}

build arm64
build x86_64

mkdir -p "$DEST/bin"
lipo -create src-arm64/bin/gs src-x86_64/bin/gs -output "$DEST/bin/gs"
codesign --force --sign - "$DEST/bin/gs"   # ad-hoc signature (required on Apple Silicon)
lipo -info "$DEST/bin/gs"
echo "--- libraries used by the bundled gs:"
otool -L "$DEST/bin/gs"

cp src-arm64/LICENSE "$DEST/COPYING.txt" 2>/dev/null || cp src-arm64/doc/COPYING "$DEST/COPYING.txt"
sed 's/unmodified Windows 64-bit binaries/built from unmodified source for macOS (universal)/' \
  "${GITHUB_WORKSPACE:-$(pwd)}/.github/ghostscript-README.txt" > "$DEST/README.txt"
"$DEST/bin/gs" --version
arch -x86_64 "$DEST/bin/gs" --version
