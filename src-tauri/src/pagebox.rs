//! Reading and editing PDF page boxes (MediaBox, CropBox, TrimBox, ArtBox,
//! BleedBox) and page rotation directly via `lopdf`, independent of
//! Ghostscript.
//!
//! We deliberately don't use Ghostscript for this: rewriting a PDF through
//! `pdfwrite` to change a box would re-distill the whole file (fonts,
//! images, content streams), which is exactly what you don't want when the
//! only thing that should change is four numbers in a page dictionary.
//! `lopdf` lets us edit just that dictionary entry and re-save.
//!
//! NOTE ON VERSION RISK: this file was written without the ability to
//! compile against crates.io (see the top-level README), so it targets
//! lopdf's documented API as of the ~0.31–0.34 range from memory. If
//! `cargo build` reports mismatches, the likely spots are:
//!   - `Dictionary::get`/`remove` taking `&[u8]` vs `&str`
//!   - `Object::Real`'s inner type (`f64` vs `f32`)
//!   - `Document::get_dictionary_mut` naming
//! Check docs.rs for the resolved lopdf version and adjust accordingly.

use lopdf::{Document, Object, ObjectId};
use serde::Serialize;
use std::path::Path;

/// The five standard PDF page boxes, in the order Acrobat's Output Preview
/// panel lists them (largest/outermost to smallest/innermost).
pub const BOX_NAMES: [&str; 5] = ["MediaBox", "CropBox", "TrimBox", "ArtBox", "BleedBox"];

#[derive(Serialize, Clone, Copy, Debug)]
pub struct PageBoxInfo {
    /// True if this box is set explicitly on the page's own dictionary
    /// (as opposed to inherited from a Pages node, or defaulted).
    pub explicit: bool,
    /// [x0, y0, x1, y1] in PDF points, in the page's own (unrotated)
    /// coordinate space.
    pub rect: [f64; 4],
    /// [left, top, width, height] as fractions (0..1) of the *displayed*
    /// page — i.e. already accounting for /Rotate — suitable for drawing
    /// an overlay directly over the rendered preview image regardless of
    /// its pixel size.
    pub norm: [f64; 4],
}

#[derive(Serialize, Debug)]
pub struct PageBoxes {
    pub rotate: i32,
    pub media: PageBoxInfo,
    pub crop: PageBoxInfo,
    pub trim: PageBoxInfo,
    pub art: PageBoxInfo,
    pub bleed: PageBoxInfo,
}

/// Page count via lopdf — used as a fallback when Ghostscript's own
/// page-count query fails.
pub fn page_count(path: &Path) -> Result<u32, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let n = doc.get_pages().len() as u32;
    if n == 0 {
        Err("This PDF has no pages.".into())
    } else {
        Ok(n)
    }
}

fn obj_to_f64(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(f) => Some(*f as f64),
        _ => None,
    }
}

fn resolve(doc: &Document, o: &Object) -> Object {
    match o {
        Object::Reference(id) => doc.get_object(*id).cloned().unwrap_or(Object::Null),
        other => other.clone(),
    }
}

fn array_to_rect(doc: &Document, o: &Object) -> Option<[f64; 4]> {
    if let Object::Array(arr) = resolve(doc, o) {
        if arr.len() == 4 {
            let vals: Vec<f64> = arr
                .iter()
                .map(|v| resolve(doc, v))
                .filter_map(|v| obj_to_f64(&v))
                .collect();
            if vals.len() == 4 {
                let x0 = vals[0].min(vals[2]);
                let x1 = vals[0].max(vals[2]);
                let y0 = vals[1].min(vals[3]);
                let y1 = vals[1].max(vals[3]);
                return Some([x0, y0, x1, y1]);
            }
        }
    }
    None
}

fn page_id_for(doc: &Document, page: u32) -> Result<ObjectId, String> {
    doc.get_pages()
        .get(&page)
        .copied()
        .ok_or_else(|| format!("Page {page} does not exist in this PDF."))
}

/// Looks for `key` on the page's own dictionary, then walks up /Parent.
/// Returns the value and whether it was found on the page itself (as
/// opposed to inherited). Used for MediaBox/CropBox/Rotate, which the PDF
/// spec allows to be inherited from an ancestor Pages node.
fn find_inherited(doc: &Document, start: ObjectId, key: &str) -> Option<(Object, bool)> {
    let mut id = start;
    let mut own = true;
    loop {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(val) = dict.get(key.as_bytes()) {
            return Some((val.clone(), own));
        }
        own = false;
        match dict.get(b"Parent").ok() {
            Some(Object::Reference(pid)) => id = *pid,
            _ => return None,
        }
    }
}

/// Looks for `key` only on the page's own dictionary — used for
/// TrimBox/ArtBox/BleedBox, which the PDF spec does *not* allow to be
/// inherited (only MediaBox and CropBox are inheritable page attributes).
fn find_own(doc: &Document, page_id: ObjectId, key: &str) -> Option<Object> {
    let dict = doc.get_dictionary(page_id).ok()?;
    dict.get(key.as_bytes()).ok().cloned()
}

fn resolve_box(doc: &Document, page_id: ObjectId, key: &str, fallback: [f64; 4]) -> ([f64; 4], bool) {
    if let Some((val, explicit)) = find_inherited(doc, page_id, key) {
        if let Some(rect) = array_to_rect(doc, &val) {
            return (rect, explicit);
        }
    }
    (fallback, false)
}

fn resolve_box_own(doc: &Document, page_id: ObjectId, key: &str, fallback: [f64; 4]) -> ([f64; 4], bool) {
    if let Some(val) = find_own(doc, page_id, key) {
        if let Some(rect) = array_to_rect(doc, &val) {
            return (rect, true);
        }
    }
    (fallback, false)
}

fn rotate_of(doc: &Document, page_id: ObjectId) -> i32 {
    find_inherited(doc, page_id, "Rotate")
        .and_then(|(o, _)| obj_to_f64(&resolve(doc, &o)))
        .map(|f| (((f as i32) % 360) + 360) % 360)
        .unwrap_or(0)
}

/// Rotates a point in a unit square (top-left origin, x right, y down) by
/// a clockwise page /Rotate of 0/90/180/270 degrees, matching how a PDF
/// viewer rotates the rendered page.
fn rotate_point(x: f64, y: f64, rotate: i32) -> (f64, f64) {
    match rotate {
        90 => (1.0 - y, x),
        180 => (1.0 - x, 1.0 - y),
        270 => (y, 1.0 - x),
        _ => (x, y),
    }
}

/// Converts a box rect (in PDF point space, y-up) into a normalized
/// [left, top, width, height] rect (0..1, y-down, post-rotation) relative
/// to the MediaBox — i.e. exactly what's needed to position an overlay
/// on top of the rendered preview image, whatever DPI it was rendered at.
fn normalize_rect(rect: [f64; 4], media: [f64; 4], rotate: i32) -> [f64; 4] {
    let mw = (media[2] - media[0]).max(1e-6);
    let mh = (media[3] - media[1]).max(1e-6);

    let to_unit = |x: f64, y: f64| -> (f64, f64) { ((x - media[0]) / mw, 1.0 - (y - media[1]) / mh) };

    let corners = [
        to_unit(rect[0], rect[3]), // top-left
        to_unit(rect[2], rect[3]), // top-right
        to_unit(rect[2], rect[1]), // bottom-right
        to_unit(rect[0], rect[1]), // bottom-left
    ];
    let rotated: Vec<(f64, f64)> = corners.iter().map(|&(x, y)| rotate_point(x, y, rotate)).collect();

    let left = rotated.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let right = rotated.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let top = rotated.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let bottom = rotated.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);

    [left, top, (right - left).max(0.0), (bottom - top).max(0.0)]
}

/// Reads the resolved page boxes (following inheritance/defaults exactly
/// as a compliant PDF consumer would) for one page.
pub fn get_page_boxes(path: &Path, page: u32) -> Result<PageBoxes, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = page_id_for(&doc, page)?;
    let rotate = rotate_of(&doc, page_id);

    // US Letter is an arbitrary but harmless fallback for the (invalid,
    // per spec) case of a page with no MediaBox anywhere in its ancestry.
    let (media_rect, media_explicit) = resolve_box(&doc, page_id, "MediaBox", [0.0, 0.0, 612.0, 792.0]);
    let (crop_rect, crop_explicit) = resolve_box(&doc, page_id, "CropBox", media_rect);
    let (trim_rect, trim_explicit) = resolve_box_own(&doc, page_id, "TrimBox", crop_rect);
    let (art_rect, art_explicit) = resolve_box_own(&doc, page_id, "ArtBox", crop_rect);
    let (bleed_rect, bleed_explicit) = resolve_box_own(&doc, page_id, "BleedBox", crop_rect);

    let mk = |rect: [f64; 4], explicit: bool| PageBoxInfo {
        explicit,
        rect,
        norm: normalize_rect(rect, media_rect, rotate),
    };

    Ok(PageBoxes {
        rotate,
        media: mk(media_rect, media_explicit),
        crop: mk(crop_rect, crop_explicit),
        trim: mk(trim_rect, trim_explicit),
        art: mk(art_rect, art_explicit),
        bleed: mk(bleed_rect, bleed_explicit),
    })
}

fn validate_box_name(name: &str) -> Result<(), String> {
    if BOX_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!("Unknown page box '{name}'."))
    }
}

/// Writes `rect` directly onto the page's own dictionary under `name`,
/// making it explicit on this page (standard behavior — this is also
/// what Acrobat's "Crop Pages" dialog does under the hood).
pub fn set_page_box(path: &Path, page: u32, name: &str, rect: [f64; 4]) -> Result<PageBoxes, String> {
    validate_box_name(name)?;
    if !(rect[2] > rect[0] && rect[3] > rect[1]) {
        return Err("Box width and height must be positive (x1 > x0 and y1 > y0).".into());
    }

    let mut doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = page_id_for(&doc, page)?;
    let arr = Object::Array(rect.iter().map(|v| Object::Real(*v as _)).collect());
    {
        let dict = doc
            .get_dictionary_mut(page_id)
            .map_err(|e| format!("Could not access the page dictionary: {e}"))?;
        dict.set(name, arr);
    }
    doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;

    get_page_boxes(path, page)
}

/// Sets the page's `/Rotate` entry (a clockwise viewing rotation applied
/// on top of the page's raw content — the same entry Acrobat's "Rotate
/// Pages" edits, and the same one Ghostscript honors when rasterizing,
/// so both the preview and the page-box overlay pick it up automatically
/// on the next render). Always written on the page's own dictionary
/// (not an ancestor), even though `/Rotate` is technically inheritable,
/// so it unambiguously applies to just this page.
pub fn set_rotation(path: &Path, page: u32, degrees: i32) -> Result<PageBoxes, String> {
    let normalized = ((degrees % 360) + 360) % 360;
    if normalized % 90 != 0 {
        return Err("Rotation must be a multiple of 90 degrees.".into());
    }

    let mut doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = page_id_for(&doc, page)?;
    {
        let dict = doc
            .get_dictionary_mut(page_id)
            .map_err(|e| format!("Could not access the page dictionary: {e}"))?;
        dict.set("Rotate", Object::Integer(normalized as i64));
    }
    doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;

    get_page_boxes(path, page)
}

/// Removes an explicit box entry from the page's own dictionary, letting
/// it fall back to inheritance/default (CropBox → MediaBox; TrimBox /
/// ArtBox / BleedBox → CropBox). MediaBox can't be reset this way since
/// every page must have one — set it to the desired value instead.
pub fn reset_page_box(path: &Path, page: u32, name: &str) -> Result<PageBoxes, String> {
    validate_box_name(name)?;
    if name == "MediaBox" {
        return Err(
            "MediaBox can't be reset to a default — every page must have one. Set it to the size you want instead."
                .into(),
        );
    }

    let mut doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = page_id_for(&doc, page)?;
    {
        let dict = doc
            .get_dictionary_mut(page_id)
            .map_err(|e| format!("Could not access the page dictionary: {e}"))?;
        dict.remove(name.as_bytes());
    }
    doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;

    get_page_boxes(path, page)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check against real files: `MP_TEST_PDFS="a.pdf;b.pdf" cargo test edit_roundtrip -- --nocapture`.
    /// Each file is copied, its TrimBox rewritten, and the result re-read.
    #[test]
    fn edit_roundtrip() {
        let Ok(list) = std::env::var("MP_TEST_PDFS") else { return };
        for src in list.split(';').filter(|s| !s.is_empty()) {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("t.pdf");
            std::fs::copy(src, &p).unwrap();
            let res = get_page_boxes(&p, 1).and_then(|b| set_page_box(&p, 1, "TrimBox", b.trim.rect));
            println!("{src}: {:?}", res.map(|b| b.trim.rect));
            if let Ok(out) = std::env::var("MP_TEST_OUT") {
                let name = Path::new(src).file_name().unwrap();
                std::fs::copy(&p, Path::new(&out).join(name)).unwrap();
            }
        }
    }
}
