# Mangoprint Prepress PDF

A free prepress checker for print-ready PDFs. Check overprint,
separations and ink coverage, see and set trim and bleed, find RGB and spot colours, and fix them, all in a native
desktop app (Tauri: Rust backend, plain JavaScript UI) with nothing else to
install on Windows.

📺 [Video walkthrough](https://youtu.be/T5qqebtV9_g)

Free and open-source, licensed [AGPL-3.0](LICENSE). Developed and
maintained by Chris Cork from [Mangoprint.co.uk](https://mangoprint.co.uk).

## Installing

Grab the latest build from
[Releases](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases).

**These builds aren't code-signed** (that needs a paid developer
certificate this project doesn't have yet), so your OS will warn that the
publisher is unverified on first launch. That's expected for unsigned beta
software, not a sign anything's wrong:

- **Windows (installer)**: [Direct download (setup .exe)](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases/download/v0.1.0-beta/Mangoprint-Prepress-PDF_0.1.0_x64-setup.exe),
  or the [.msi](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases/download/v0.1.0-beta/Mangoprint-Prepress-PDF_0.1.0_x64_en-US.msi)
  if you prefer. SmartScreen will show "Windows protected your PC" — click
  **More info**, then **Run anyway**.
- **Windows (portable, no install)**: [Direct download (.zip)](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases/download/v0.1.0-beta/Mangoprint-Prepress-PDF_0.1.0_portable_win64.zip).
  Unzip anywhere (a USB stick is fine) and run
  `Mangoprint Prepress PDF.exe`. Keep the `ghostscript` folder next to it.
- **Linux**: [Direct download (.AppImage)](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases/download/v0.1.0-beta/Mangoprint-Prepress-PDF_0.1.0_amd64.AppImage).
  `chmod +x` the `.AppImage` and run it directly (works on Debian, Ubuntu,
  Arch, Fedora and most others):
  ```
  chmod +x Mangoprint-Prepress-PDF_0.1.0_amd64.AppImage
  ./Mangoprint-Prepress-PDF_0.1.0_amd64.AppImage
  ```
  Or install the [.deb](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases/download/v0.1.0-beta/Mangoprint-Prepress-PDF_0.1.0_amd64.deb)
  / [.rpm](https://github.com/crispin81/Mangoprint-Prepress-PDF/releases/download/v0.1.0-beta/Mangoprint-Prepress-PDF-0.1.0-1.x86_64.rpm)
  for your distro. On Wayland the AppImage runs natively, falling back to
  X11 automatically if needed.
- **macOS**: coming soon.

Ghostscript, which powers overprint preview, separations, ink readings and
the colour/font conversions, is **built into every download** — there's nothing else to install.
Windows 10 and 11 already include the WebView2 runtime the app needs.

## Usage

1. **Open a PDF** with the Open PDF button (or click the file name in the
   toolbar to open another). Pages appear as thumbnails on the left; the page
   itself is drawn as true vectors, sharp at any zoom, with images at their
   own resolution.
2. **Check it.**
   - **Overprint Preview** — tick *Simulate overprinting* to see what an
     overprint-aware press will actually produce.
   - **Separations** — hover over the page to read the ink % of every
     plate (CMYK and spot colours, plus total ink) at the cursor; click to
     lock a reading (a gold padlock follows the cursor), click again to
     release. Untick plates to inspect trapping and knockouts.
   - **Raster DPI of PDF** in the toolbar shows the effective resolution of
     the placed images on the page, plus *Vector* when there's vector art
     or text.
   - **Select text** (on by default) lets you highlight live text, so you
     can tell text from outlined artwork.
   - A **white overprint warning** pops up on opening if anything white is
     set to overprint (it would vanish on press).
3. **Fix it.** Everything in the gold *Apply edits to* box works on all
   pages by default, or switch to *This page*:
   - **Colour & Fonts** — check for RGB, convert RGB to CMYK, convert spot
     colours to CMYK, convert text to outlines.
   - **Page Rotation** — or use the ↺ ↻ arrows on a single thumbnail.
   - **Page Boxes** — trim (red) and bleed (blue) measured in mm in from
     each edge of the page; the lines move as you type.
4. **Export PDF** saves a copy with your changes.

### Keyboard and mouse

- <kbd>Z</kbd> — zoom area tool: drag a box to zoom into it, click to zoom
  in, <kbd>Alt</kbd>+click to zoom out
- <kbd>Ctrl</kbd>+mouse wheel, <kbd>Ctrl</kbd>+<kbd>+</kbd>/<kbd>−</kbd> — zoom;
  <kbd>Ctrl</kbd>+<kbd>0</kbd> — fit page; <kbd>Ctrl</kbd>+<kbd>1</kbd> — actual size
- Mouse wheel, <kbd>Page Up</kbd>/<kbd>Page Down</kbd>, <kbd>Home</kbd>/<kbd>End</kbd> — move between pages
- <kbd>Esc</kbd> — leave the zoom tool / text selection

## Data safety

- **Your original PDF is never modified.** Opening a file makes a working
  copy in a temporary folder; every edit and conversion goes to that copy.
  Nothing reaches your files until you choose **Export PDF**, which writes
  a new file (named `…_edited.pdf` by default). The app asks before
  discarding unexported changes.
- **Conversions keep quality**: images stay at full resolution (no
  downsampling, JPEGs passed through untouched), existing CMYK values,
  overprint settings and page boxes are preserved, and vectors stay vectors.
- **CMYK values are shown as they are in the file** — embedded colour
  profiles aren't applied to CMYK, so 100% K reads as 100% K, not a
  converted rich black.

## Building / running

Requires Node 18+ and a Rust toolchain (see [tauri.app/start/prerequisites](https://tauri.app/start/prerequisites/)),
plus [Ghostscript](https://ghostscript.com/releases/gsdnld.html) for
development.

```bash
npm install
npm run tauri dev      # run in development
npm run tauri build    # produce a native installer for the current OS
```

Every build bundles Ghostscript from `src-tauri/ghostscript/`. On Windows,
copy `bin/gswin64c.exe`, `bin/gsdll64.dll` and `doc/COPYING` (as
`COPYING.txt`) from a Ghostscript 10.x install; on Linux and macOS run
`.github/scripts/build-ghostscript-linux.sh` or `-macos.sh`, which compile a
self-contained Ghostscript from source. The GitHub Actions workflows do this
automatically.

`tauri build` produces a `.msi`/`.exe` on Windows, `.dmg`/`.app` on macOS
and `.AppImage`/`.deb`/`.rpm` on Linux — run it on each target OS (or via
CI) to get that platform's installer.

## Credits

Built with [Ghostscript](https://ghostscript.com) (Artifex Software,
AGPL-3.0), [PDF.js](https://github.com/mozilla/pdf.js) (Mozilla,
Apache-2.0), [Tauri](https://tauri.app) (MIT/Apache-2.0),
[lopdf](https://github.com/J-F-Liu/lopdf) (MIT) and the Poppins typeface
(SIL Open Font License). See [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

If it saves you time, [buy Chris a coffee](https://buymeacoffee.com/chriscorkphotography) 😊
