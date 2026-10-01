# Mangoprint Prepress PDF

A small desktop app for **Windows, macOS and Linux** (Tauri v2 + Rust +
vanilla JS) for checking trapping in print-ready PDFs: overprint
simulation, per-separation (CMYK + spot) plate inspection, and page
box/rotation checking and editing, in the spirit of Adobe Acrobat's
**Output Preview** panel. Styled to match RapidCulling (bundled Poppins,
same dark palette and gold accent, frameless title bar).

It does not reimplement a RIP. It drives **Ghostscript**, the engine most
real prepress tooling is built on, and asks it the two questions
Acrobat's panel answers:

1. **Overprint Preview** — render the page with `-dSimulateOverprint=true`
   vs `false`. This is the literal flag behind Acrobat's "Simulate
   Overprinting" checkbox: off shows the naive composite where each object
   knocks out what's underneath it; on shows what will actually happen on
   an overprint-aware output device.
2. **Separations** — render through Ghostscript's `tiffsep` device, which
   rasterizes every colorant (C, M, Y, K, and any spot colors defined in
   the PDF) to its own 8-bit grayscale plate, already resolved through the
   page's real overprint/knockout state. The app lets you toggle plates
   on/off and recombines the checked ones (in Rust) using a standard
   subtractive print model, so you can see e.g. whether black text is set
   to overprint or knockout — the classic trapping check.

## Installing (users)

1. **Install Ghostscript** — the app's only runtime dependency. It's not
   bundled (Ghostscript is AGPL-licensed and large), so install it once:
   - **Windows:** run the 64-bit installer from
     https://ghostscript.com/releases/gsdnld.html. The app finds it in
     `C:\Program Files\gs\…` automatically; no PATH changes needed.
   - **macOS:** `brew install ghostscript` (Homebrew and MacPorts locations
     are found automatically).
   - **Linux:** `sudo apt install ghostscript` / `sudo dnf install
     ghostscript` / `sudo pacman -S ghostscript`.

   The app shows a red banner at the top if it can't find Ghostscript.
2. **Install the app** from the installers built by GitHub (see below):
   - Windows: the `.exe` (NSIS) or `.msi`. WebView2 is already part of
     Windows 11; on older Windows the installer fetches it.
   - macOS: the `.dmg` (universal — Apple Silicon and Intel). It isn't
     signed with an Apple Developer ID, so the first time, right-click the
     app → **Open** → **Open** (or run `xattr -cr "/Applications/Mangoprint
     Prepress PDF.app"`).
   - Linux: `.AppImage`, `.deb` or `.rpm`.

## Building installers for all three platforms (GitHub Actions)

You don't need a Mac or a Windows PC to build their versions. Push this
project to a GitHub repo and:

- **Any push to `main`** (or **Actions → build → Run workflow**) builds
  Windows, macOS and Linux installers. When the run finishes, download them
  from the **Artifacts** section at the bottom of the run's page
  (`Mangoprint-Windows`, `Mangoprint-macOS`, `Mangoprint-Linux`).
- **Pushing a tag** like `v0.1.0` (`git tag v0.1.0 && git push --tags`)
  builds all three and attaches the installers to a **draft Release** you
  can review and publish.

Same setup as RapidCulling's workflows, including its patched
`linuxdeploy` GTK hook for native Wayland in the AppImage.

## Building locally (developers)

Needs Rust (`rustup`, stable), Node.js 18+, and Ghostscript, plus:

- **Windows:** Microsoft C++ Build Tools ("Desktop development with C++").
  WebView2 is already present on Windows 10/11.
- **macOS:** Xcode Command Line Tools (`xcode-select --install`).
- **Linux:** WebKitGTK and friends, e.g. Debian/Ubuntu:
  `sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev`.
  Full list: https://v2.tauri.app/start/prerequisites/

```bash
npm install
npm run dev      # run the app in dev mode
npm run build    # build installers for the platform you're on
cargo test --manifest-path src-tauri/Cargo.toml   # unit tests
```

There's no frontend build step: `src/` is plain HTML/CSS/JS served as-is,
using Tauri's global `window.__TAURI__` API.

## How to use it

1. **Open PDF…** and pick a print-ready file.
2. Use **Overprint Preview** mode and flip **Simulate overprinting** to
   compare the two renders — a mismatch usually means an overprint flag
   was set incorrectly somewhere in the file.
3. Switch to **Separations** mode to isolate individual plates:
   - Uncheck everything but K: is small black text built from K only, or
     does it pull in C/M/Y too?
   - Uncheck K: does removing black leave a gap (knockout) or does the
     artwork underneath remain (overprint)?
   - Spot colors are listed by name below the process plates — toggle them
     to confirm they're where you expect (e.g. a die-line or varnish plate).
4. **Page Rotation** sets the page's `/Rotate` entry (0°/90°/180°/270°),
   the same thing Acrobat's "Rotate Pages" changes.
5. **Page Boxes** shows MediaBox, CropBox, TrimBox, ArtBox and BleedBox
   for the current page as colored outlines over the preview. You can see
   whether each is explicit or defaulted, edit its coordinates (points),
   or reset it to default. Edits are written straight into the page
   dictionary via `lopdf` (no re-distilling) and saved immediately; a
   `<filename>.pdf.bak` safety copy is made the first time you edit a file.

## Known limitations / approximations

- **Spot color preview is not color-accurate** — spot plates show as a
  neutral darkening (placement and coverage, not the real ink color).
- **No ICC-managed CMYK→RGB conversion** — for structural/trapping checks,
  not exact soft-proofing.
- Rendering is per-page on demand; large pages at 300 DPI take a few
  seconds (Ghostscript is the bottleneck).
- A malformed Trim/Art/Bleed box entry is treated as "not set" rather than
  flagged as an error.
- Editing a page box rewrites the whole file via `lopdf`.
- The window is frameless (custom title bar, like RapidCulling), so macOS
  shows the app's own minimize/maximize/close buttons rather than the
  usual traffic lights.

## Project layout

```
src/                        plain HTML/CSS/JS frontend + bundled Poppins fonts
src-tauri/src/gs.rs         Ghostscript discovery (per OS) + invocation
src-tauri/src/pagebox.rs    page box + rotation read/write via lopdf
src-tauri/src/lib.rs        Tauri commands, separation caching, plate recombination
src-tauri/icons/            app icons (.png, .ico for Windows, .icns for macOS)
.github/workflows/          build.yml (3-platform artifacts), release.yml (tagged releases)
```

## Build status

The first real compile happens on the first GitHub Actions run (or your
first `npm run dev`). The Ghostscript module has been compiled and
unit-tested on its own; the rest hasn't been compiled yet. If something
fails, the likeliest spots are `src-tauri/src/pagebox.rs` (`lopdf` API
differences between versions — see the note at the top of that file) and
`src-tauri/capabilities/default.json` (Tauri permission names).
