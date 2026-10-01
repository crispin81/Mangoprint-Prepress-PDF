mod colorcheck;
mod gs;
mod imageres;
mod overprint;
mod pagebox;
mod spotcolor;

use base64::Engine;
use image::{GrayImage, RgbImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

/// Caches per (path, page, dpi) separation renders so toggling plate
/// checkboxes in the UI doesn't re-invoke Ghostscript every time — only
/// the recombination step (cheap, pure Rust) reruns. Keys are plain
/// "<path>\0<page>\0<dpi>" strings (not hashed) so that editing a page
/// box can cheaply invalidate every cached render for that file — see
/// `invalidate_path`.
struct SepCache(Mutex<HashMap<String, TempDir>>);

fn cache_key(pdf_path: &str, page: u32, dpi: u32) -> String {
    format!("{pdf_path}\0{page}\0{dpi}")
}

/// Drops every cached separation render for `pdf_path`, regardless of
/// page or DPI. Called after a page box or rotation edit, since those can
/// change the page's rendered dimensions and would otherwise leave stale
/// (wrong-size) cached plates behind.
fn invalidate_path(cache: &SepCache, pdf_path: &str) {
    let prefix = format!("{pdf_path}\0");
    if let Ok(mut guard) = cache.0.lock() {
        guard.retain(|k, _| !k.starts_with(&prefix));
    }
}

/// Returns the cache directory for a (path, page, dpi) key, creating it
/// if this is the first request for that key.
fn cache_dir(cache: &SepCache, key: String) -> Result<PathBuf, String> {
    let mut guard = cache
        .0
        .lock()
        .map_err(|_| "Separation cache was poisoned".to_string())?;
    if let Some(dir) = guard.get(&key) {
        return Ok(dir.path().to_path_buf());
    }
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let p = tmp.path().to_path_buf();
    guard.insert(key, tmp);
    Ok(p)
}

/// Decoded separation plates for the page currently being sampled by the
/// eyedropper, keyed like `SepCache`. Only one page is held at a time.
struct PlateCache(Mutex<Option<(String, Vec<(String, GrayImage)>)>>);

#[derive(serde::Serialize)]
struct InkSample {
    name: String,
    /// Ink coverage 0–100 %.
    percent: f32,
}

/// The temp folder holding the working copy of the currently open PDF.
/// Geometry edits go to this copy; the original file is never touched
/// until the user exports. Replacing it drops (deletes) the previous copy.
struct WorkingCopy(Mutex<Option<TempDir>>);

fn png_to_data_uri(png_path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(png_path).map_err(|e| format!("Could not read rendered PNG: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

#[tauri::command]
fn check_ghostscript() -> Result<String, String> {
    gs::check_available()
}

/// Page count comes from Ghostscript (it's what renders the pages), with
/// lopdf as a fallback in case the Ghostscript query is blocked or fails.
#[tauri::command]
fn open_pdf(path: String) -> Result<u32, String> {
    let p = Path::new(&path);
    match gs::page_count(p) {
        Ok(n) => Ok(n),
        Err(gs_err) => pagebox::page_count(p).map_err(|_| gs_err),
    }
}

/// Copies `path` into a fresh temp folder (keeping its file name) and
/// returns the copy's path, which the UI then uses for everything.
#[tauri::command]
fn create_working_copy(path: String, working: tauri::State<WorkingCopy>) -> Result<String, String> {
    let src = Path::new(&path);
    let name = src.file_name().ok_or("Invalid file path")?;
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let dest = tmp.path().join(name);
    std::fs::copy(src, &dest).map_err(|e| format!("Could not copy the PDF: {e}"))?;
    *working.0.lock().map_err(|_| "Working copy lock was poisoned".to_string())? = Some(tmp);
    Ok(dest.to_string_lossy().into_owned())
}

/// Raw PDF bytes for the in-app vector renderer (pdf.js). Returned as a
/// binary IPC response so the frontend gets an ArrayBuffer, not JSON.
#[tauri::command]
fn read_pdf(path: String) -> Result<tauri::ipc::Response, String> {
    std::fs::read(&path)
        .map(tauri::ipc::Response::new)
        .map_err(|e| format!("Could not read the PDF: {e}"))
}

/// Writes the edited working copy out to `dest`.
#[tauri::command]
fn export_pdf(from: String, dest: String) -> Result<(), String> {
    if Path::new(&from) == Path::new(&dest) {
        return Ok(());
    }
    std::fs::copy(&from, &dest)
        .map(|_| ())
        .map_err(|e| format!("Could not save the PDF to {dest}: {e}"))
}

#[tauri::command]
fn render_overprint(path: String, page: u32, dpi: u32, simulate: bool) -> Result<String, String> {
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let out_png = tmp.path().join("preview.png");
    gs::render_overprint_png(Path::new(&path), page, simulate, dpi, &out_png)?;
    png_to_data_uri(&out_png)
}

/// Runs (or reuses a cached run of) tiffsep for the given page and returns
/// the colorant names found, in process-then-spot order.
#[tauri::command]
fn list_separations(path: String, page: u32, dpi: u32, cache: tauri::State<SepCache>) -> Result<Vec<String>, String> {
    let dir_path = cache_dir(&cache, cache_key(&path, page, dpi))?;
    let found = gs::ensure_separations(Path::new(&path), page, dpi, &dir_path)?;
    Ok(found.into_iter().map(|(n, _)| n).collect())
}

/// Recombines the checked separation plates into a preview image using a
/// standard subtractive (print) compositing model. Process colorants
/// (Cyan/Magenta/Yellow/Black) are combined with the textbook formula
/// R = 255*(1-C)*(1-K), G = 255*(1-M)*(1-K), B = 255*(1-Y)*(1-K). Spot
/// colorants have no fixed RGB equivalent (their real appearance depends
/// on the PDF's alternate/tint-transform color space, which this tool
/// does not evaluate), so they're approximated as a neutral darkening —
/// enough to see where a spot plate has ink and how it traps against the
/// process plates, but not a color-accurate spot preview.
#[tauri::command]
fn render_separation_composite(
    path: String,
    page: u32,
    dpi: u32,
    active: Vec<String>,
    // Spot name -> "#rrggbb" display colour (from spot_swatches); spots
    // without one are shown as a neutral darkening.
    spot_colours: Option<HashMap<String, String>>,
    cache: tauri::State<SepCache>,
) -> Result<String, String> {
    let spot_colours = spot_colours.unwrap_or_default();
    let parse_hex = |h: &str| -> Option<[f32; 3]> {
        let h = h.strip_prefix('#')?;
        let v = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(|b| b as f32 / 255.0);
        Some([v(0)?, v(2)?, v(4)?])
    };
    let dir_path = cache_dir(&cache, cache_key(&path, page, dpi))?;
    let plates = gs::ensure_separations(Path::new(&path), page, dpi, &dir_path)?;
    let active_set: std::collections::HashSet<&str> = active.iter().map(|s| s.as_str()).collect();

    let mut process: HashMap<&str, GrayImage> = HashMap::new();
    let mut spots: Vec<(GrayImage, [f32; 3])> = Vec::new();
    let mut dims: Option<(u32, u32)> = None;

    for (name, tif_path) in &plates {
        if !active_set.contains(name.as_str()) {
            continue;
        }
        let img = image::open(tif_path)
            .map_err(|e| format!("Could not read separation plate '{name}': {e}"))?
            .to_luma8();
        dims.get_or_insert((img.width(), img.height()));
        match name.as_str() {
            "Cyan" | "Magenta" | "Yellow" | "Black" => {
                process.insert(name.as_str(), img);
            }
            _ => {
                // Default: neutral grey at 80% darkening.
                let colour = spot_colours.get(name).and_then(|h| parse_hex(h)).unwrap_or([0.2, 0.2, 0.2]);
                spots.push((img, colour));
            }
        }
    }

    let (w, h) = match dims {
        Some(d) => d,
        None => {
            // Nothing selected: return a blank white page at the native
            // plate resolution so the UI has something sane to show.
            let (_, first) = &plates[0];
            image::open(first).map(|i| i.to_luma8().dimensions()).unwrap_or((1, 1))
        }
    };

    let ink = |img: &HashMap<&str, GrayImage>, name: &str, x: u32, y: u32| -> f32 {
        img.get(name)
            .map(|g| 1.0 - (g.get_pixel(x, y).0[0] as f32 / 255.0))
            .unwrap_or(0.0)
    };

    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let c = ink(&process, "Cyan", x, y);
            let m = ink(&process, "Magenta", x, y);
            let ye = ink(&process, "Yellow", x, y);
            let k = ink(&process, "Black", x, y);

            let mut r = 255.0 * (1.0 - c) * (1.0 - k);
            let mut g = 255.0 * (1.0 - m) * (1.0 - k);
            let mut b = 255.0 * (1.0 - ye) * (1.0 - k);

            // Each spot multiplies in its own colour, scaled by its tint.
            for (spot, [sr, sg, sb]) in &spots {
                let s = 1.0 - (spot.get_pixel(x, y).0[0] as f32 / 255.0);
                r *= 1.0 - s * (1.0 - sr);
                g *= 1.0 - s * (1.0 - sg);
                b *= 1.0 - s * (1.0 - sb);
            }

            out.put_pixel(x, y, image::Rgb([r.round() as u8, g.round() as u8, b.round() as u8]));
        }
    }

    let tmp_out = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let out_path = tmp_out.path().join("composite.png");
    out.save(&out_path)
        .map_err(|e| format!("Could not encode preview PNG: {e}"))?;
    png_to_data_uri(&out_path)
}

#[derive(serde::Serialize)]
struct PageContent {
    /// [lowest, highest] effective raster image resolution in whole PPI;
    /// `None` when the page has no raster images.
    ppi: Option<[u32; 2]>,
    /// The page has vector content (paths, shadings or text).
    vector: bool,
}

#[tauri::command]
fn page_image_dpi(path: String, page: u32) -> Result<PageContent, String> {
    let scan = imageres::scan_page(Path::new(&path), page)?;
    Ok(PageContent {
        ppi: scan.ppi.map(|(lo, hi)| [lo.round() as u32, hi.round() as u32]),
        vector: scan.vector,
    })
}

/// Eyedropper: ink coverage of every separation at a point on the page.
/// `x`/`y` are fractions (0–1) of the displayed page. Values come from
/// Ghostscript's tiffsep plates, so they reflect real overprint behaviour.
#[tauri::command]
fn sample_inks(
    path: String,
    page: u32,
    dpi: u32,
    x: f64,
    y: f64,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<Vec<InkSample>, String> {
    let key = cache_key(&path, page, dpi);
    let mut guard = plates.0.lock().map_err(|_| "Plate cache was poisoned".to_string())?;
    if guard.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
        let dir_path = cache_dir(&cache, key.clone())?;
        let found = gs::ensure_separations(Path::new(&path), page, dpi, &dir_path)?;
        let mut decoded = Vec::with_capacity(found.len());
        for (name, tif) in found {
            let img = image::open(&tif)
                .map_err(|e| format!("Could not read separation plate '{name}': {e}"))?
                .to_luma8();
            decoded.push((name, img));
        }
        *guard = Some((key, decoded));
    }
    let (_, decoded) = guard.as_ref().expect("plate cache just filled");
    Ok(decoded
        .iter()
        .map(|(name, img)| {
            let px = ((x.clamp(0.0, 1.0) * img.width() as f64) as u32).min(img.width().saturating_sub(1));
            let py = ((y.clamp(0.0, 1.0) * img.height() as f64) as u32).min(img.height().saturating_sub(1));
            let v = img.get_pixel(px, py).0[0] as f32;
            InkSample { name: name.clone(), percent: (1.0 - v / 255.0) * 100.0 }
        })
        .collect())
}

fn clear_plates(plates: &PlateCache) {
    if let Ok(mut g) = plates.0.lock() {
        *g = None;
    }
}

/// White objects set to overprint (empty when there are none).
#[tauri::command]
fn check_overprint(path: String) -> Result<Vec<overprint::OverprintHit>, String> {
    overprint::check_overprint(Path::new(&path))
}

/// Spot colour name → "#rrggbb" swatch at 100% tint.
#[tauri::command]
fn spot_swatches(path: String) -> Result<std::collections::BTreeMap<String, String>, String> {
    spotcolor::spot_swatches(Path::new(&path))
}

/// Spot colour names used anywhere in the PDF.
#[tauri::command]
fn spot_colours(path: String) -> Result<Vec<String>, String> {
    colorcheck::spot_names(Path::new(&path))
}

/// Pages that contain RGB objects (empty when the file is RGB-free).
#[tauri::command]
fn check_rgb(path: String) -> Result<Vec<colorcheck::PageRgb>, String> {
    colorcheck::check_rgb(Path::new(&path))
}

/// Applies a whole-document fix ("outlines", "rgb", "spots") to the working
/// copy in place, via a temp file so a failed conversion leaves it intact.
#[tauri::command]
fn convert_pdf(
    path: String,
    kind: String,
    // Some(page) = convert just that page; None = whole document.
    page: Option<u32>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<(), String> {
    let conv = match kind.as_str() {
        "outlines" => gs::Conversion::Outlines,
        "rgb" => gs::Conversion::RgbToCmyk,
        "spots" => gs::Conversion::SpotsToCmyk,
        other => return Err(format!("Unknown conversion '{other}'")),
    };
    let src = Path::new(&path);
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let out = tmp.path().join("converted.pdf");
    match page {
        Some(p) => {
            let count = gs::page_count(src).or_else(|_| pagebox::page_count(src))?;
            gs::convert_one_page(src, &out, conv, p, count)?
        }
        None => gs::convert_pdf(src, &out, conv)?,
    }
    std::fs::copy(&out, src).map_err(|e| format!("Could not update the working copy: {e}"))?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(())
}

#[tauri::command]
fn get_page_boxes(path: String, page: u32) -> Result<pagebox::PageBoxes, String> {
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command]
fn set_page_box(
    path: String,
    page: u32,
    name: String,
    rect: [f64; 4],
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<pagebox::PageBoxes, String> {
    let result = pagebox::set_page_box(Path::new(&path), page, &name, rect)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

/// Applies a box to every page as edge distances (points: top, bottom,
/// left, right); returns the current page's boxes afterwards.
#[tauri::command]
fn set_page_box_all(
    path: String,
    page: u32,
    name: String,
    insets: [f64; 4],
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<pagebox::PageBoxes, String> {
    pagebox::set_box_insets_all(Path::new(&path), &name, insets)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command]
fn set_rotation_all(
    path: String,
    page: u32,
    degrees: i32,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<pagebox::PageBoxes, String> {
    pagebox::set_rotation_all(Path::new(&path), degrees)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command]
fn reset_page_box_all(
    path: String,
    page: u32,
    name: String,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<pagebox::PageBoxes, String> {
    pagebox::reset_box_all(Path::new(&path), &name)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command]
fn reset_page_box(
    path: String,
    page: u32,
    name: String,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<pagebox::PageBoxes, String> {
    let result = pagebox::reset_page_box(Path::new(&path), page, &name)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

#[tauri::command]
fn set_page_rotation(
    path: String,
    page: u32,
    degrees: i32,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
) -> Result<pagebox::PageBoxes, String> {
    let result = pagebox::set_rotation(Path::new(&path), page, degrees)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(SepCache(Mutex::new(HashMap::new())))
        .manage(WorkingCopy(Mutex::new(None)))
        .manage(PlateCache(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            check_ghostscript,
            open_pdf,
            create_working_copy,
            read_pdf,
            export_pdf,
            render_overprint,
            list_separations,
            render_separation_composite,
            page_image_dpi,
            check_rgb,
            spot_colours,
            spot_swatches,
            check_overprint,
            convert_pdf,
            sample_inks,
            get_page_boxes,
            set_page_box,
            reset_page_box,
            set_page_box_all,
            set_rotation_all,
            reset_page_box_all,
            set_page_rotation,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
