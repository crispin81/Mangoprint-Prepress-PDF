//! Thin wrapper around the Ghostscript CLI. This module is the only place
//! that shells out, and the only place that needs to know Ghostscript's
//! quirks (PostScript string escaping, tiffsep filename conventions,
//! per-platform binary names, etc).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Searches `PATH` for the first of `names` that exists as a file.
fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Windows: the official Ghostscript installer puts the console binary at
/// `C:\Program Files\gs\gs10.xx.x\bin\gswin64c.exe` and does NOT add it to
/// PATH, so we look there too, preferring the newest installed version.
#[cfg(windows)]
fn find_platform_specific() -> Option<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(v) = std::env::var_os(var) {
            roots.push(PathBuf::from(v).join("gs"));
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in roots {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for e in entries.flatten() {
                for exe in ["gswin64c.exe", "gswin32c.exe"] {
                    let p = e.path().join("bin").join(exe);
                    if p.is_file() {
                        candidates.push(p);
                    }
                }
            }
        }
    }
    // Directory names are like "gs10.04.0"; lexical sort is close enough
    // to version order for picking the newest.
    candidates.sort();
    candidates.pop()
}

/// macOS: apps launched from Finder/Dock don't inherit the shell's PATH,
/// so a Homebrew/MacPorts install wouldn't be found via PATH alone.
#[cfg(target_os = "macos")]
fn find_platform_specific() -> Option<PathBuf> {
    ["/opt/homebrew/bin/gs", "/usr/local/bin/gs", "/opt/local/bin/gs"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_platform_specific() -> Option<PathBuf> {
    ["/usr/bin/gs", "/usr/local/bin/gs", "/snap/bin/gs"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

#[cfg(windows)]
const PATH_NAMES: &[&str] = &["gswin64c.exe", "gswin32c.exe", "gs.exe"];
#[cfg(not(windows))]
const PATH_NAMES: &[&str] = &["gs"];

#[cfg(windows)]
const INSTALL_HINT: &str =
    "Download and run the Windows installer from https://ghostscript.com/releases/gsdnld.html, then restart this app.";
#[cfg(target_os = "macos")]
const INSTALL_HINT: &str = "Install it with Homebrew (`brew install ghostscript`), then restart this app.";
#[cfg(all(unix, not(target_os = "macos")))]
const INSTALL_HINT: &str =
    "Install it with your package manager, e.g. `sudo apt install ghostscript` or `sudo dnf install ghostscript`.";

/// Locates Ghostscript once per run and caches the result.
fn gs_binary() -> Result<&'static Path, String> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| find_on_path(PATH_NAMES).or_else(find_platform_specific))
        .as_deref()
        .ok_or_else(|| format!("Ghostscript was not found. {INSTALL_HINT}"))
}

/// Builds a `Command` for Ghostscript. On Windows it also suppresses the
/// console window that would otherwise flash up for every render.
fn gs_command() -> Result<Command, String> {
    #[allow(unused_mut)] // only mutated on Windows
    let mut cmd = Command::new(gs_binary()?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

/// Returns the Ghostscript version string, or an error explaining that
/// Ghostscript is missing. Called at startup so the UI can show a clear
/// message instead of failing confusingly on first render.
pub fn check_available() -> Result<String, String> {
    let out = gs_command()?
        .arg("--version")
        .output()
        .map_err(|e| format!("Ghostscript could not be started ({e}). {INSTALL_HINT}"))?;
    if !out.status.success() {
        return Err("Ghostscript is installed but returned an error when queried for its version.".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Escapes a filesystem path for embedding inside a PostScript literal
/// string, i.e. `(...)`. Backslashes (Windows separators included) and
/// parentheses must be escaped or Ghostscript's tokenizer will misparse it.
fn ps_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

/// Ghostscript treats `%` in -sOutputFile as a printf-style page-number
/// placeholder; a literal `%` must be doubled.
/// Prepress wants the CMYK numbers that are actually in the file. By
/// default Ghostscript colour-manages ICC-based CMYK (embedded profiles)
/// into its own default CMYK profile, which shifts values — e.g. 100 K
/// text turns into a rich black. `OverrideICC` makes it ignore embedded
/// profiles so CMYK passes straight through; plain DeviceCMYK is already
/// passed through untouched. RGB content still has to be converted.
const PRESERVE_CMYK: &str = "-dOverrideICC=true";

fn output_file_arg(path: &Path) -> String {
    format!("-sOutputFile={}", path.to_string_lossy().replace('%', "%%"))
}

/// Returns the number of pages in the PDF by asking Ghostscript's own PDF
/// interpreter (`pdfpagecount`). Ghostscript runs with -dSAFER by default
/// since 9.50, which blocks PostScript `file` access, so the PDF is
/// explicitly allow-listed with --permit-file-read (in both separator
/// styles on Windows) rather than turning SAFER off.
pub fn page_count(pdf_path: &Path) -> Result<u32, String> {
    let escaped = ps_escape(pdf_path);
    let raw = pdf_path.to_string_lossy().to_string();
    let script = format!("({escaped}) (r) file runpdfbegin pdfpagecount = quit");
    let out = gs_command()?
        .args(["-q", "-dNODISPLAY", "-dBATCH", "-dNOPAUSE"])
        .arg(format!("--permit-file-read={raw}"))
        .arg(format!("--permit-file-read={}", raw.replace('\\', "/")))
        .args(["-c", &script])
        .output()
        .map_err(|e| format!("Failed to run Ghostscript: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!("Could not determine page count. Ghostscript said:\n{stderr}")
        })
}

/// Renders one page to a flat RGB PNG, either with or without Ghostscript's
/// overprint simulation. This mirrors Acrobat's "Simulate Overprinting"
/// checkbox in Output Preview: with `simulate = false` you get the naive
/// composite (each object's colors knock out what's beneath it); with
/// `simulate = true` you get what will actually happen on an overprint-aware
/// device, i.e. inks that were left set to overprint blend with what's
/// underneath instead of masking it.
pub fn render_overprint_png(
    pdf_path: &Path,
    page: u32,
    simulate: bool,
    dpi: u32,
    out_png: &Path,
) -> Result<(), String> {
    let status = gs_command()?
        .args([
            "-q",
            "-dBATCH",
            "-dNOPAUSE",
            "-dSAFER",
            "-sDEVICE=png16m",
            PRESERVE_CMYK,
            &format!("-r{dpi}"),
            &format!("-dFirstPage={page}"),
            &format!("-dLastPage={page}"),
            &format!("-dSimulateOverprint={}", if simulate { "true" } else { "false" }),
            &output_file_arg(out_png),
        ])
        .arg(pdf_path)
        .output()
        .map_err(|e| format!("Failed to run Ghostscript: {e}"))?;

    if !status.status.success() || !out_png.exists() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(format!("Ghostscript failed to render the page:\n{stderr}"));
    }
    Ok(())
}

/// Extracts the colorant name from a tiffsep plate filename. Ghostscript
/// names plates `<base>(<Colorant>).tif`, e.g. `sep(Cyan).tif` or
/// `sep(PANTONE 186 C).tif`; the older `sep.Cyan.tif` form is accepted too.
fn plate_name(fname: &str) -> Option<&str> {
    fname
        .strip_prefix("sep(")
        .and_then(|s| s.strip_suffix(").tif"))
        .or_else(|| fname.strip_prefix("sep.").and_then(|s| s.strip_suffix(".tif")))
        .filter(|s| !s.is_empty())
}

/// Scans a directory already populated by `render_separations` and
/// returns the separation plates it contains, without invoking
/// Ghostscript again. Used to serve cached results cheaply.
fn scan_separations(out_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut found = Vec::new();
    let entries =
        std::fs::read_dir(out_dir).map_err(|e| format!("Could not read Ghostscript's output directory: {e}"))?;
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname == "sep.tif" {
            continue; // the 32-bit CMYK composite, not a plate
        }
        if let Some(name) = plate_name(&fname) {
            found.push((name.to_string(), entry.path()));
        }
    }

    // Sort with a stable, print-industry-conventional order: process inks
    // first (CMYK), then any spot colorants alphabetically.
    let rank = |name: &str| -> (u8, String) {
        match name {
            "Cyan" => (0, name.to_string()),
            "Magenta" => (1, name.to_string()),
            "Yellow" => (2, name.to_string()),
            "Black" => (3, name.to_string()),
            other => (4, other.to_string()),
        }
    };
    found.sort_by_key(|(name, _)| rank(name));
    Ok(found)
}

/// Renders one page through the `tiffsep` device, which rasterizes each
/// colorant (process C/M/Y/K plus any spot separations found in the PDF)
/// to its own 8-bit grayscale TIFF, resolved through the page's actual
/// overprint/knockout state. Always invokes Ghostscript; prefer
/// `ensure_separations` when a previous render may already be cached.
pub fn render_separations(
    pdf_path: &Path,
    page: u32,
    dpi: u32,
    out_dir: &Path,
) -> Result<Vec<(String, PathBuf)>, String> {
    let composite = out_dir.join("sep.tif");
    let status = gs_command()?
        .args([
            "-q",
            "-dBATCH",
            "-dNOPAUSE",
            "-dSAFER",
            "-sDEVICE=tiffsep",
            PRESERVE_CMYK,
            &format!("-r{dpi}"),
            &format!("-dFirstPage={page}"),
            &format!("-dLastPage={page}"),
            &output_file_arg(&composite),
        ])
        .arg(pdf_path)
        .output()
        .map_err(|e| format!("Failed to run Ghostscript: {e}"))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(format!("Ghostscript failed to separate the page:\n{stderr}"));
    }

    let found = scan_separations(out_dir)?;
    if found.is_empty() {
        return Err("Ghostscript produced no separation plates for this page.".into());
    }
    Ok(found)
}

/// Like `render_separations`, but if `out_dir` already holds a completed
/// render (marked by the presence of the composite `sep.tif`), reuses it
/// instead of shelling out to Ghostscript again. This is what makes
/// toggling plate checkboxes in the UI fast after the first render of a
/// given page/DPI.
pub fn ensure_separations(
    pdf_path: &Path,
    page: u32,
    dpi: u32,
    out_dir: &Path,
) -> Result<Vec<(String, PathBuf)>, String> {
    if out_dir.join("sep.tif").exists() {
        let found = scan_separations(out_dir)?;
        if !found.is_empty() {
            return Ok(found);
        }
    }
    render_separations(pdf_path, page, dpi, out_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plate_names() {
        assert_eq!(plate_name("sep(Cyan).tif"), Some("Cyan"));
        assert_eq!(plate_name("sep(PANTONE 186 C).tif"), Some("PANTONE 186 C"));
        assert_eq!(plate_name("sep.Black.tif"), Some("Black"));
        assert_eq!(plate_name("sep.tif"), None);
        assert_eq!(plate_name("other.png"), None);
    }

    #[test]
    fn escaping() {
        assert_eq!(ps_escape(Path::new(r"C:\a (1)\b.pdf")), r"C:\\a \(1\)\\b.pdf");
        assert_eq!(
            output_file_arg(Path::new("/tmp/100%.png")),
            "-sOutputFile=/tmp/100%%.png"
        );
    }
}
