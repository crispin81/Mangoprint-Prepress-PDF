mod barcode;
mod colorcheck;
mod fonts;
mod gs;
mod history;
mod iccprofiles;
mod imageres;
mod impose;
mod objects;
mod overprint;
mod pagebox;
mod pages;
mod recolour;
mod spotcolor;
mod sysfonts;

use base64::Engine;
use image::{GrayImage, RgbImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tempfile::TempDir;

/// Caches per (path, page, dpi) separation renders so toggling plate
/// checkboxes in the UI doesn't re-invoke Ghostscript every time — only
/// the recombination step (cheap, pure Rust) reruns. Keys are plain
/// "<path>\0<page>\0<dpi>" strings (not hashed) so that editing a page
/// box can cheaply invalidate every cached render for that file — see
/// `invalidate_path`.
struct SepCache(Mutex<HashMap<String, (TempDir, Arc<Mutex<()>>)>>);

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
/// if this is the first request for that key, plus the lock that stops two
/// requests rendering into it at once.
fn cache_dir(cache: &SepCache, key: String) -> Result<(PathBuf, Arc<Mutex<()>>), String> {
    let mut guard = cache
        .0
        .lock()
        .map_err(|_| "Separation cache was poisoned".to_string())?;
    if let Some((dir, lock)) = guard.get(&key) {
        return Ok((dir.path().to_path_buf(), lock.clone()));
    }
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let p = tmp.path().to_path_buf();
    let lock = Arc::new(Mutex::new(()));
    guard.insert(key, (tmp, lock.clone()));
    Ok((p, lock))
}

/// The page's separation plates, rendered once and then reused. Requests
/// for the same page wait for each other rather than rendering twice.
fn separations(cache: &SepCache, path: &str, page: u32, dpi: u32) -> Result<Vec<(String, PathBuf)>, String> {
    let (dir_path, lock) = cache_dir(cache, cache_key(path, page, dpi))?;
    let _rendering = lock.lock().map_err(|_| "Separation render lock was poisoned".to_string())?;
    gs::ensure_separations(Path::new(path), page, dpi, &dir_path)
}

/// Commands run on background threads (so a slow job such as rendering
/// separations never holds up the page). Reads of the working copy share
/// this lock; edits take it exclusively, so nothing reads a half-written
/// file.
struct DocLock(RwLock<()>);

fn read_lock(doc: &DocLock) -> Result<std::sync::RwLockReadGuard<'_, ()>, String> {
    doc.0.read().map_err(|_| "Document lock was poisoned".to_string())
}

fn write_lock(doc: &DocLock) -> Result<std::sync::RwLockWriteGuard<'_, ()>, String> {
    doc.0.write().map_err(|_| "Document lock was poisoned".to_string())
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

/// The temp folders holding the working copies of the open PDFs (one per
/// tab), keyed by the copy's path. Edits go to these copies; the original
/// files are never touched until the user saves. Closing a tab drops
/// (deletes) its copy.
struct WorkingCopy(Mutex<HashMap<String, TempDir>>);

fn png_to_data_uri(png_path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(png_path).map_err(|e| format!("Could not read rendered PNG: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

#[tauri::command(async)]
fn check_ghostscript() -> Result<String, String> {
    gs::check_available()
}

/// Page count from lopdf (fast, no extra process), with Ghostscript as a
/// fallback for files lopdf can't parse.
#[tauri::command(async)]
fn open_pdf(path: String, doc: tauri::State<DocLock>) -> Result<u32, String> {
    let _r = read_lock(&doc)?;
    let p = Path::new(&path);
    match pagebox::page_count(p) {
        Ok(n) if n > 0 => Ok(n),
        _ => gs::page_count(p),
    }
}

/// Copies `path` into a fresh temp folder (keeping its file name) and
/// returns the copy's path, which the UI then uses for everything.
#[tauri::command(async)]
fn create_working_copy(
    app: tauri::AppHandle,
    path: String,
    working: tauri::State<WorkingCopy>,
) -> Result<String, String> {
    let src = Path::new(&path);
    let name = src.file_name().ok_or("Invalid file path")?;
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let dest = tmp.path().join(name);
    std::fs::copy(src, &dest).map_err(|e| format!("Could not copy the PDF: {e}"))?;
    // The viewer reads the copy straight from disk, a piece at a time, so
    // a large PDF shows without first passing the whole file over.
    {
        use tauri::Manager;
        let _ = app.asset_protocol_scope().allow_file(&dest);
    }
    let dest = dest.to_string_lossy().into_owned();
    working.0.lock().map_err(|_| "Working copy lock was poisoned".to_string())?.insert(dest.clone(), tmp);
    Ok(dest)
}

/// Deletes a closed tab's working copy once nothing is still reading it
/// (e.g. its separations), without making the app wait for that.
#[tauri::command(async)]
fn close_working_copy(
    app: tauri::AppHandle,
    path: String,
    working: tauri::State<WorkingCopy>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
) -> Result<(), String> {
    hist.forget(&path);
    let old = working.0.lock().map_err(|_| "Working copy lock was poisoned".to_string())?.remove(&path);
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    if let Some(old) = old {
        std::thread::spawn(move || {
            use tauri::Manager;
            let doc = app.state::<DocLock>();
            let _w = write_lock(&doc);
            drop(old);
        });
    }
    Ok(())
}

/// Raw PDF bytes for the in-app vector renderer (pdf.js). Returned as a
/// binary IPC response so the frontend gets an ArrayBuffer, not JSON.
#[tauri::command(async)]
fn read_pdf(path: String, doc: tauri::State<DocLock>) -> Result<tauri::ipc::Response, String> {
    let _r = read_lock(&doc)?;
    std::fs::read(&path)
        .map(tauri::ipc::Response::new)
        .map_err(|e| format!("Could not read the PDF: {e}"))
}

/// Writes the edited working copy out to `dest`.
#[tauri::command(async)]
fn export_pdf(
    from: String,
    dest: String,
    // The mail merge boxes not yet made into pages, saved inside the PDF so
    // reopening it brings them back; None clears any saved setup.
    merge_setup: Option<String>,
    doc: tauri::State<DocLock>,
) -> Result<(), String> {
    let _r = read_lock(&doc)?;
    if Path::new(&from) != Path::new(&dest) {
        std::fs::copy(&from, &dest).map_err(|e| format!("Could not save the PDF to {dest}: {e}"))?;
    }
    pages::write_merge_setup(Path::new(&dest), merge_setup.as_deref())
}

/// The mail merge setup saved in the PDF, if any.
#[tauri::command(async)]
fn merge_setup_read(path: String, doc: tauri::State<DocLock>) -> Result<Option<String>, String> {
    let _r = read_lock(&doc)?;
    pages::read_merge_setup(Path::new(&path))
}

#[tauri::command(async)]
fn render_overprint(
    path: String,
    page: u32,
    dpi: u32,
    simulate: bool,
    doc: tauri::State<DocLock>,
) -> Result<String, String> {
    let _r = read_lock(&doc)?;
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let out_png = tmp.path().join("preview.png");
    gs::render_overprint_png(Path::new(&path), page, simulate, dpi, &out_png)?;
    png_to_data_uri(&out_png)
}

/// Runs (or reuses a cached run of) tiffsep for the given page and returns
/// the colorant names found, in process-then-spot order.
#[tauri::command(async)]
fn list_separations(
    path: String,
    page: u32,
    dpi: u32,
    cache: tauri::State<SepCache>,
    doc: tauri::State<DocLock>,
) -> Result<Vec<String>, String> {
    let _r = read_lock(&doc)?;
    let found = separations(&cache, &path, page, dpi)?;
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
#[tauri::command(async)]
fn render_separation_composite(
    path: String,
    page: u32,
    dpi: u32,
    // The plates to show; None = all of them.
    active: Option<Vec<String>>,
    // Spot name -> "#rrggbb" display colour (from spot_swatches); spots
    // without one are shown as a neutral darkening.
    spot_colours: Option<HashMap<String, String>>,
    // Spot inks cover what's under them (like metallic or opaque inks)
    // instead of mixing with it.
    opaque_spots: Option<bool>,
    cache: tauri::State<SepCache>,
    doc: tauri::State<DocLock>,
) -> Result<String, String> {
    let spot_colours = spot_colours.unwrap_or_default();
    let opaque = opaque_spots.unwrap_or(false);
    let parse_hex = |h: &str| -> Option<[f32; 3]> {
        let h = h.strip_prefix('#')?;
        let v = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(|b| b as f32 / 255.0);
        Some([v(0)?, v(2)?, v(4)?])
    };
    let plates = {
        let _r = read_lock(&doc)?;
        separations(&cache, &path, page, dpi)?
    };
    let active = active.unwrap_or_else(|| plates.iter().map(|(n, _)| n.clone()).collect());
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

            // Each spot multiplies in its own colour, scaled by its tint;
            // an opaque spot instead covers what's underneath.
            for (spot, [sr, sg, sb]) in &spots {
                let s = 1.0 - (spot.get_pixel(x, y).0[0] as f32 / 255.0);
                if opaque {
                    r = r * (1.0 - s) + 255.0 * sr * s;
                    g = g * (1.0 - s) + 255.0 * sg * s;
                    b = b * (1.0 - s) + 255.0 * sb * s;
                } else {
                    r *= 1.0 - s * (1.0 - sr);
                    g *= 1.0 - s * (1.0 - sg);
                    b *= 1.0 - s * (1.0 - sb);
                }
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
struct InkLimit {
    /// Transparent PNG, bright green where total ink is over the limit.
    image: String,
    /// Highest total ink on the page, %.
    max: f32,
    /// Share of the page over the limit, 0–1.
    over: f32,
}

/// Where the page's total ink (all plates added up) is over `limit` %,
/// from the same tiffsep plates as the ink readings.
#[tauri::command(async)]
fn ink_limit_overlay(
    path: String,
    page: u32,
    dpi: u32,
    limit: f32,
    cache: tauri::State<SepCache>,
    doc: tauri::State<DocLock>,
) -> Result<InkLimit, String> {
    let plates = {
        let _r = read_lock(&doc)?;
        separations(&cache, &path, page, dpi)?
    };
    let mut images = Vec::with_capacity(plates.len());
    for (name, tif) in &plates {
        let img = image::open(tif)
            .map_err(|e| format!("Could not read separation plate '{name}': {e}"))?
            .to_luma8();
        images.push(img);
    }
    let (w, h) = images.first().map(|i| i.dimensions()).ok_or("The page has no separations.")?;
    // Plate values are 255 = no ink, 0 = 100%; compare in those units.
    let limit_units = limit / 100.0 * 255.0;
    let mut out = image::RgbaImage::new(w, h);
    let (mut max, mut over) = (0u32, 0u64);
    for y in 0..h {
        for x in 0..w {
            let total: u32 = images
                .iter()
                .filter(|i| i.dimensions() == (w, h))
                .map(|i| 255 - i.get_pixel(x, y).0[0] as u32)
                .sum();
            max = max.max(total);
            if total as f32 > limit_units + 0.5 {
                over += 1;
                out.put_pixel(x, y, image::Rgba([0, 255, 0, 255]));
            }
        }
    }
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let out_path = tmp.path().join("inklimit.png");
    out.save(&out_path).map_err(|e| format!("Could not encode the ink limit PNG: {e}"))?;
    Ok(InkLimit {
        image: png_to_data_uri(&out_path)?,
        max: max as f32 / 255.0 * 100.0,
        over: over as f32 / (w as f32 * h as f32),
    })
}

#[derive(serde::Serialize)]
struct PageContent {
    /// [lowest, highest] effective raster image resolution in whole PPI;
    /// `None` when the page has no raster images.
    ppi: Option<[u32; 2]>,
    /// The page has vector content (paths, shadings or text).
    vector: bool,
}

#[tauri::command(async)]
fn page_image_dpi(path: String, page: u32, doc: tauri::State<DocLock>) -> Result<PageContent, String> {
    let _r = read_lock(&doc)?;
    let scan = imageres::scan_page(Path::new(&path), page)?;
    Ok(PageContent {
        ppi: scan.ppi.map(|(lo, hi)| [lo.round() as u32, hi.round() as u32]),
        vector: scan.vector,
    })
}

/// Eyedropper: ink coverage of every separation at a point on the page.
/// `x`/`y` are fractions (0–1) of the displayed page. Values come from
/// Ghostscript's tiffsep plates, so they reflect real overprint behaviour.
#[tauri::command(async)]
fn sample_inks(
    path: String,
    page: u32,
    dpi: u32,
    x: f64,
    y: f64,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    doc: tauri::State<DocLock>,
) -> Result<Vec<InkSample>, String> {
    let key = cache_key(&path, page, dpi);
    let _r = read_lock(&doc)?;
    let mut guard = plates.0.lock().map_err(|_| "Plate cache was poisoned".to_string())?;
    if guard.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
        let found = separations(&cache, &path, page, dpi)?;
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
#[tauri::command(async)]
fn check_overprint(path: String, doc: tauri::State<DocLock>) -> Result<Vec<overprint::OverprintHit>, String> {
    let _r = read_lock(&doc)?;
    overprint::check_overprint(Path::new(&path))
}

/// Spot colour name → "#rrggbb" swatch at 100% tint.
#[tauri::command(async)]
fn spot_swatches(
    path: String,
    doc: tauri::State<DocLock>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let _r = read_lock(&doc)?;
    spotcolor::spot_swatches(Path::new(&path))
}

/// Fonts used, with whether each is embedded and the pages it is on.
#[tauri::command(async)]
fn list_fonts(path: String, doc: tauri::State<DocLock>) -> Result<Vec<fonts::FontInfo>, String> {
    let _r = read_lock(&doc)?;
    fonts::list_fonts(Path::new(&path))
}

/// Imposition: the layout the settings would give (for the summary line).
#[tauri::command(async)]
fn impose_plan(path: String, spec: impose::ImposeSpec, doc: tauri::State<DocLock>) -> Result<impose::ImposePlan, String> {
    let _r = read_lock(&doc)?;
    impose::plan(Path::new(&path), &spec)
}

#[derive(serde::Serialize)]
struct Imposed {
    /// The imposed PDF, in a temp folder, to open as a new tab.
    path: String,
    plan: impose::ImposePlan,
}

/// Imposition: writes the open PDF laid out on press sheets to a new file
/// named `name` in a fresh temp folder (the working copy is unchanged).
#[tauri::command(async)]
fn impose_pdf(path: String, name: String, spec: impose::ImposeSpec, doc: tauri::State<DocLock>) -> Result<Imposed, String> {
    let _r = read_lock(&doc)?;
    let dir = std::env::temp_dir().join("mangoprint-impose").join(format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create a temp folder: {e}"))?;
    let safe: String = name.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).collect();
    let out = dir.join(if safe.is_empty() { "imposed.pdf".to_string() } else { safe });
    let plan = impose::impose(Path::new(&path), &out, &spec)?;
    Ok(Imposed { path: out.to_string_lossy().into_owned(), plan })
}

/// ICC colour profiles embedded in the PDF (output intent and colour spaces).
#[tauri::command(async)]
fn list_icc_profiles(path: String, doc: tauri::State<DocLock>) -> Result<Vec<iccprofiles::IccProfile>, String> {
    let _r = read_lock(&doc)?;
    iccprofiles::list_profiles(Path::new(&path))
}

/// Soft proof of a page through a CMYK profile: `profile_id` = one embedded
/// in the PDF ("12 0"), or `profile_file` = an .icc file the user chose.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn render_proof(
    path: String,
    page: u32,
    dpi: u32,
    simulate: bool,
    profile_id: Option<String>,
    profile_file: Option<String>,
    paper: bool,
    doc: tauri::State<DocLock>,
) -> Result<String, String> {
    let _r = read_lock(&doc)?;
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let profile = match (profile_id, profile_file) {
        (Some(id), _) => {
            let p = tmp.path().join("proof.icc");
            std::fs::write(&p, iccprofiles::profile_bytes(Path::new(&path), &id)?)
                .map_err(|e| format!("Could not write the profile: {e}"))?;
            p
        }
        (None, Some(f)) => PathBuf::from(f),
        (None, None) => return Err("Choose a profile to proof with.".into()),
    };
    let out_png = tmp.path().join("proof.png");
    gs::render_proof_png(Path::new(&path), page, simulate, dpi, &profile, paper, &out_png)?;
    png_to_data_uri(&out_png)
}

/// Sets the PDF's output intent to an embedded profile (`profile_id`) or
/// an .icc file (`profile_file`), or removes it when neither is given.
/// Colour values aren't changed. Undoable like any other edit.
#[tauri::command(async)]
fn set_output_intent(
    path: String,
    profile_id: Option<String>,
    profile_file: Option<String>,
    // Convert the colours from the old output intent to the new one.
    convert: Option<bool>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<bool, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let source = match (profile_id, profile_file) {
        (Some(id), _) => iccprofiles::IntentSource::Embedded(id),
        (None, Some(f)) => iccprofiles::IntentSource::File(PathBuf::from(f)),
        (None, None) => iccprofiles::IntentSource::None,
    };
    let converted = iccprofiles::change_output_intent(Path::new(&path), source, convert.unwrap_or(false))?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(converted)
}

/// Header details of an .icc file the user chose, to check it can proof.
#[tauri::command(async)]
fn read_icc_file(path: String) -> Result<iccprofiles::IccProfile, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("Could not read the profile: {e}"))?;
    iccprofiles::describe_bytes(&bytes).ok_or_else(|| "That file isn't an ICC colour profile.".to_string())
}

/// Saves one embedded ICC profile (`id` from list_icc_profiles) as a file.
#[tauri::command(async)]
fn save_icc_profile(path: String, id: String, dest: String, doc: tauri::State<DocLock>) -> Result<(), String> {
    let bytes = {
        let _r = read_lock(&doc)?;
        iccprofiles::profile_bytes(Path::new(&path), &id)?
    };
    std::fs::write(&dest, bytes).map_err(|e| format!("Could not save the profile: {e}"))
}

/// Spot colour names used anywhere in the PDF.
#[tauri::command(async)]
fn spot_colours(path: String, doc: tauri::State<DocLock>) -> Result<Vec<String>, String> {
    let _r = read_lock(&doc)?;
    colorcheck::spot_names(Path::new(&path))
}

/// Pages that contain RGB objects (empty when the file is RGB-free).
#[tauri::command(async)]
fn check_rgb(path: String, doc: tauri::State<DocLock>) -> Result<Vec<colorcheck::PageRgb>, String> {
    let _r = read_lock(&doc)?;
    colorcheck::check_rgb(Path::new(&path))
}

/// Applies a whole-document fix ("outlines", "rgb", "spots") to the working
/// copy in place, via a temp file so a failed conversion leaves it intact.
#[tauri::command(async)]
fn convert_pdf(
    path: String,
    kind: String,
    // Some(page) = convert just that page; None = whole document.
    page: Option<u32>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<(), String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let conv = match kind.as_str() {
        "outlines" => gs::Conversion::Outlines,
        "rgb" => gs::Conversion::RgbToCmyk,
        "spots" => gs::Conversion::SpotsToCmyk,
        other => return Err(format!("Unknown conversion '{other}'")),
    };
    let tmp = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let out = tmp.path().join("converted.pdf");
    // Outlined text stays grouped line by line (see group_text_blocks).
    let grouped = tmp.path().join("grouped.pdf");
    let src = if matches!(conv, gs::Conversion::Outlines) && objects::group_text_blocks(Path::new(&path), &grouped).is_ok() {
        grouped.as_path()
    } else {
        Path::new(&path)
    };
    match page {
        Some(p) => {
            let count = gs::page_count(src).or_else(|_| pagebox::page_count(src))?;
            gs::convert_one_page(src, &out, conv, p, count)?
        }
        None => gs::convert_pdf(src, &out, conv)?,
    }
    std::fs::copy(&out, &path).map_err(|e| format!("Could not update the working copy: {e}"))?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(())
}

#[tauri::command(async)]
fn get_page_boxes(path: String, page: u32, doc: tauri::State<DocLock>) -> Result<pagebox::PageBoxes, String> {
    let _r = read_lock(&doc)?;
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command(async)]
fn set_page_box(
    path: String,
    page: u32,
    name: String,
    rect: [f64; 4],
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<pagebox::PageBoxes, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let result = pagebox::set_page_box(Path::new(&path), page, &name, rect)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

/// Applies a box to every page as edge distances (points: top, bottom,
/// left, right); returns the current page's boxes afterwards.
#[tauri::command(async)]
fn set_page_box_all(
    path: String,
    page: u32,
    name: String,
    insets: [f64; 4],
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<pagebox::PageBoxes, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    pagebox::set_box_insets_all(Path::new(&path), &name, insets)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command(async)]
fn set_rotation_all(
    path: String,
    page: u32,
    degrees: i32,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<pagebox::PageBoxes, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    pagebox::set_rotation_all(Path::new(&path), degrees)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command(async)]
fn reset_page_box_all(
    path: String,
    page: u32,
    name: String,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<pagebox::PageBoxes, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    pagebox::reset_box_all(Path::new(&path), &name)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    pagebox::get_page_boxes(Path::new(&path), page)
}

#[tauri::command(async)]
fn reset_page_box(
    path: String,
    page: u32,
    name: String,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<pagebox::PageBoxes, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let result = pagebox::reset_page_box(Path::new(&path), page, &name)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

#[tauri::command(async)]
fn set_page_rotation(
    path: String,
    page: u32,
    degrees: i32,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<pagebox::PageBoxes, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let result = pagebox::set_rotation(Path::new(&path), page, degrees)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

/// Recolour tool: the object under the cursor (x/y are fractions of the
/// displayed page, `tol` the hit tolerance in points).
#[tauri::command(async)]
fn recolour_pick(
    path: String,
    page: u32,
    x: f64,
    y: f64,
    tol: f64,
    doc: tauri::State<DocLock>,
) -> Result<Option<recolour::Picked>, String> {
    let _r = read_lock(&doc)?;
    recolour::pick(Path::new(&path), page, x, y, tol)
}

/// Recolour tool: what a recolour would change (count, pages, outlines on
/// the current page).
#[tauri::command(async)]
fn recolour_find(
    path: String,
    page: u32,
    target: recolour::PaintRef,
    mode: recolour::Mode,
    all_pages: bool,
    doc: tauri::State<DocLock>,
) -> Result<recolour::Found, String> {
    let _r = read_lock(&doc)?;
    recolour::find(Path::new(&path), page, target, mode, all_pages)
}

/// Recolour tool: repaints the chosen objects in CMYK (values 0–100).
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn recolour_apply(
    path: String,
    page: u32,
    target: recolour::PaintRef,
    mode: recolour::Mode,
    all_pages: bool,
    fill: Option<[f64; 4]>,
    stroke: Option<[f64; 4]>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<usize, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let n = recolour::apply_recolour(Path::new(&path), page, target, mode, all_pages, fill, stroke)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(n)
}

/// Select tool: the object under the cursor.
#[tauri::command(async)]
fn object_pick(path: String, page: u32, x: f64, y: f64, tol: f64, doc: tauri::State<DocLock>) -> Result<Option<objects::ObjInfo>, String> {
    let _r = read_lock(&doc)?;
    objects::pick(Path::new(&path), page, x, y, tol)
}

/// Select tool: objects wholly inside a box (fractions of the displayed page).
#[tauri::command(async)]
fn objects_in_rect(path: String, page: u32, rect: [f64; 4], doc: tauri::State<DocLock>) -> Result<Vec<objects::ObjInfo>, String> {
    let _r = read_lock(&doc)?;
    objects::in_rect(Path::new(&path), page, rect)
}

/// Select tool: the current outlines and colours of selected objects
/// (after another tool has changed them).
#[tauri::command(async)]
fn objects_info(path: String, page: u32, indices: Vec<usize>, doc: tauri::State<DocLock>) -> Result<Vec<objects::ObjInfo>, String> {
    let _r = read_lock(&doc)?;
    objects::info(Path::new(&path), page, &indices)
}

/// Select tool: moves, deletes or sets overprint on the selected objects.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn objects_edit(
    path: String,
    page: u32,
    indices: Vec<usize>,
    action: String,
    dx: Option<f64>,
    dy: Option<f64>,
    on: Option<bool>,
    name: Option<String>,
    cmyk: Option<[f64; 4]>,
    fill: Option<[f64; 4]>,
    stroke: Option<[f64; 4]>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<objects::EditResult, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let p = Path::new(&path);
    let result = match action.as_str() {
        "move" => objects::move_objects(p, page, &indices, dx.unwrap_or(0.0), dy.unwrap_or(0.0))?,
        "delete" => objects::delete_objects(p, page, &indices)?,
        "overprint" => objects::set_overprint(p, page, &indices, on.unwrap_or(true))?,
        "colour" => objects::recolour_objects(p, page, &indices, fill, stroke)?,
        "spot" => objects::to_spot(p, page, &indices, name.as_deref().unwrap_or(""), cmyk.unwrap_or([0.0, 0.0, 0.0, 100.0]))?,
        other => return Err(format!("Unknown edit '{other}'")),
    };
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(result)
}

/// Inserts the pages of other PDFs after the first `at` pages; returns the
/// new page count.
#[tauri::command(async)]
fn insert_pages(
    path: String,
    sources: Vec<String>,
    at: u32,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<u32, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let n = pages::insert_pdfs(Path::new(&path), &sources, at)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(n)
}

/// Moves page `from` to position `to` (1-based).
#[tauri::command(async)]
fn move_page(
    path: String,
    from: u32,
    to: u32,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<(), String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    pages::move_page(Path::new(&path), from, to)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(())
}

/// Deletes pages; returns the new page count.
#[tauri::command(async)]
fn delete_page(
    path: String,
    pages: Vec<u32>,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<u32, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let n = pages::delete_pages(Path::new(&path), &pages)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(n)
}

/// Mail merge: the rows of a CSV file, split into columns.
#[tauri::command(async)]
fn merge_read_csv(path: String, sheet: Option<String>) -> Result<Vec<Vec<String>>, String> {
    pages::read_csv(Path::new(&path), sheet.as_deref())
}

/// Mail merge: the sheets of a spreadsheet (empty for a CSV file).
#[tauri::command(async)]
fn merge_sheets(path: String) -> Result<Vec<String>, String> {
    pages::sheet_names(Path::new(&path))
}

/// Mail merge preview: the modules of one barcode.
#[tauri::command]
fn barcode_matrix(kind: String, text: String) -> Result<barcode::Matrix, String> {
    barcode::encode(&kind, text.trim())
}

/// Mail merge: installed fonts that can be embedded.
#[tauri::command(async)]
fn system_fonts() -> Vec<sysfonts::SysFont> {
    sysfonts::list()
}

/// Mail merge: replaces the page with one copy per row; returns how many.
#[tauri::command(async)]
fn mail_merge(
    path: String,
    page: u32,
    rows: Vec<Vec<String>>,
    spec: pages::MergeSpec,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<u32, String> {
    let _w = write_lock(&doc)?;
    hist.snapshot(&path)?;
    let n = pages::mail_merge(Path::new(&path), page, &rows, &spec)?;
    invalidate_path(&cache, &path);
    clear_plates(&plates);
    Ok(n)
}

/// Ctrl+Z / Ctrl+Y: steps the working copy back (`undo`) or forward
/// through its edits. Returns false when there was nothing to step to.
#[tauri::command(async)]
fn undo_step(
    path: String,
    undo: bool,
    cache: tauri::State<SepCache>,
    plates: tauri::State<PlateCache>,
    hist: tauri::State<history::History>,
    doc: tauri::State<DocLock>,
) -> Result<bool, String> {
    let _w = write_lock(&doc)?;
    let changed = hist.step(&path, undo)?;
    if changed {
        invalidate_path(&cache, &path);
        clear_plates(&plates);
    }
    Ok(changed)
}

/// How many undo and redo steps there are.
#[tauri::command]
fn undo_counts(path: String, hist: tauri::State<history::History>) -> history::Counts {
    hist.counts(&path)
}

/// PDFs handed to the app by the operating system (double-click, "Open
/// with", dropping onto the app icon) waiting for the page to pick them up.
/// Queued rather than sent straight to the page so a file given at launch
/// isn't lost before the page has loaded.
struct PendingOpen(Mutex<Vec<String>>);

/// The PDF paths among command-line arguments (Windows/Linux pass the
/// double-clicked file this way).
fn pdf_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    args.into_iter()
        .filter(|a| a.to_lowercase().ends_with(".pdf") && Path::new(a).is_file())
        .collect()
}

/// Queues PDFs to open, tells the page, and brings the window to the front.
fn hand_over(app: &tauri::AppHandle, paths: Vec<String>) {
    use tauri::{Emitter, Manager};
    if paths.is_empty() {
        return;
    }
    if let Ok(mut q) = app.state::<PendingOpen>().0.lock() {
        q.extend(paths);
    }
    let _ = app.emit("open-files", ());
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Files waiting to be opened (emptied by the call).
#[tauri::command]
fn take_pending_open(pending: tauri::State<PendingOpen>) -> Vec<String> {
    pending.0.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // One window only: opening another PDF while the app is running
        // (double-click, "Open with") sends it to the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            hand_over(app, pdf_args(argv.into_iter().skip(1)));
        }))
        .manage(PendingOpen(Mutex::new(Vec::new())))
        .setup(|app| {
            use tauri::Manager;
            if let Ok(dir) = app.path().resource_dir() {
                gs::set_resource_dir(dir);
            }
            // A PDF double-clicked to launch the app (Windows/Linux).
            hand_over(app.handle(), pdf_args(std::env::args().skip(1)));
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(SepCache(Mutex::new(HashMap::new())))
        .manage(WorkingCopy(Mutex::new(HashMap::new())))
        .manage(PlateCache(Mutex::new(None)))
        .manage(DocLock(RwLock::new(())))
        .manage(history::History::new())
        .invoke_handler(tauri::generate_handler![
            check_ghostscript,
            open_pdf,
            create_working_copy,
            close_working_copy,
            object_pick,
            objects_in_rect,
            objects_edit,
            objects_info,
            insert_pages,
            delete_page,
            move_page,
            undo_step,
            undo_counts,
            merge_read_csv,
            merge_sheets,
            merge_setup_read,
            barcode_matrix,
            mail_merge,
            system_fonts,
            read_pdf,
            export_pdf,
            render_overprint,
            list_separations,
            render_separation_composite,
            ink_limit_overlay,
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
            recolour_pick,
            recolour_find,
            recolour_apply,
            list_fonts,
            list_icc_profiles,
            impose_plan,
            impose_pdf,
            save_icc_profile,
            render_proof,
            read_icc_file,
            set_output_intent,
            take_pending_open,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, _event| {
            // macOS hands over double-clicked / "Open with" files as an event
            // rather than as command-line arguments.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = _event {
                let paths: Vec<String> = urls
                    .into_iter()
                    .filter_map(|u| u.to_file_path().ok())
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                hand_over(_app, pdf_args(paths));
            }
        });
}


#[cfg(test)]
mod plate_stats {
    /// Mean ink of each plate in MP_SEP_DIR (if set).
    #[test]
    fn plate_means() {
        let Ok(dir) = std::env::var("MP_SEP_DIR") else { return };
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let img = image::open(&p).unwrap();
            let g = img.to_luma8();
            let mean: f64 = g.pixels().map(|p| 255.0 - p.0[0] as f64).sum::<f64>() / (g.width() * g.height()) as f64 / 2.55;
            println!("{:?} {:?} {}x{} mean ink {mean:.1}%", p.file_name().unwrap(), img.color(), g.width(), g.height());
        }
    }
}

#[cfg(test)]
mod outline_repro {
    /// MP_OUTLINE_IN -> MP_OUTLINE_OUT the way the Outlines button does it.
    #[test]
    fn outline_like_app() {
        let (Ok(src), Ok(out)) = (std::env::var("MP_OUTLINE_IN"), std::env::var("MP_OUTLINE_OUT")) else { return };
        let tmp = tempfile::TempDir::new().unwrap();
        let grouped = tmp.path().join("grouped.pdf");
        crate::objects::group_text_blocks(std::path::Path::new(&src), &grouped).unwrap();
        std::fs::copy(&grouped, format!("{out}.grouped.pdf")).unwrap();
        crate::gs::convert_pdf(&grouped, std::path::Path::new(&out), crate::gs::Conversion::Outlines).unwrap();
    }
}
