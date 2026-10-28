//! Imposition: lays a PDF's pages out on press sheets, as vectors (each
//! page becomes a form XObject placed in a grid), with crop marks in a
//! registration colour that prints on every plate.
//!
//! Layouts:
//! - "repeat": each page fills its own sheet (business cards: a 2-page
//!   front/back PDF gives a front sheet and a back sheet that line up when
//!   turned, as the grid is centred).
//! - "nup": pages in order across the sheets.
//! - "cutstack": pages run down the stack, so each pile is in order once
//!   the sheets are cut (numbered or mail-merged items).

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MM: f64 = 72.0 / 25.4;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImposeSpec {
    /// "repeat", "nup" or "cutstack".
    pub mode: String,
    /// Press sheet, mm (either orientation; the better one is used).
    pub sheet_w: f64,
    pub sheet_h: f64,
    /// Space the press can't print on, every edge, mm.
    pub margin: f64,
    /// Gap between items beside their short edges, mm.
    pub gutter_short: f64,
    /// Gap between items beside their long edges, mm.
    pub gutter_long: f64,
    /// Bleed kept around each item, mm.
    pub bleed: f64,
    /// Turn items 90° when more fit that way.
    pub allow_rotate: bool,
    pub crop_marks: bool,
}

/// The layout a spec gives, for the dialog's summary line.
#[derive(Serialize, Debug, Clone)]
pub struct ImposePlan {
    pub cols: u32,
    pub rows: u32,
    pub per_sheet: u32,
    pub sheets: u32,
    pub rotated: bool,
    /// Sheet as used, mm (landscape or portrait).
    pub sheet_w: f64,
    pub sheet_h: f64,
    /// Finished item size (trim), mm, as placed.
    pub item_w: f64,
    pub item_h: f64,
    pub pages: u32,
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(f) => Some(*f as f64),
        _ => None,
    }
}

fn rect(doc: &Document, page: &Dictionary, key: &[u8]) -> Option<[f64; 4]> {
    let o = page.get(key).ok()?;
    let o = match o {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        o => o,
    };
    let a = o.as_array().ok()?;
    let v: Vec<f64> = a.iter().filter_map(num).collect();
    (v.len() == 4).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

/// A page's boxes as needed for placing it: media, trim, bleed (pt) and
/// its /Rotate.
struct PageInfo {
    id: ObjectId,
    media: [f64; 4],
    trim: [f64; 4],
    rotate: i64,
}

fn page_info(doc: &Document, id: ObjectId) -> Result<PageInfo, String> {
    let d = doc.get_dictionary(id).map_err(|e| format!("Could not read a page: {e}"))?;
    let media = rect(doc, d, b"MediaBox").unwrap_or([0.0, 0.0, 595.0, 842.0]);
    let crop = rect(doc, d, b"CropBox").unwrap_or(media);
    let trim = rect(doc, d, b"TrimBox").unwrap_or(crop);
    let rotate = d.get(b"Rotate").ok().and_then(num).map(|r| (r as i64).rem_euclid(360)).unwrap_or(0);
    Ok(PageInfo { id, media, trim, rotate })
}

/// Trim size as displayed (after /Rotate), pt.
fn display_size(p: &PageInfo) -> (f64, f64) {
    let (w, h) = (p.trim[2] - p.trim[0], p.trim[3] - p.trim[1]);
    if p.rotate == 90 || p.rotate == 270 { (h, w) } else { (w, h) }
}

type M = [f64; 6];

/// `a` then `b` (PDF matrices: row vector times matrix).
fn mul(a: M, b: M) -> M {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

/// Page space → upright display space with the trim's corner at 0,0.
fn display_matrix(p: &PageInfo) -> M {
    let [x0, y0, x1, y1] = p.trim;
    let (w, h) = (x1 - x0, y1 - y0);
    match p.rotate {
        90 => [0.0, -1.0, 1.0, 0.0, -y0, x0 + w],
        180 => [-1.0, 0.0, 0.0, -1.0, x0 + w, y0 + h],
        270 => [0.0, 1.0, -1.0, 0.0, y0 + h, -x0],
        _ => [1.0, 0.0, 0.0, 1.0, -x0, -y0],
    }
}

/// The gaps (pt) between columns and between rows for an item placed
/// `iw` × `ih`: side by side, items meet along their vertical edges, so the
/// column gap is the short-edge gap when those edges are the short ones.
fn gaps(spec: &ImposeSpec, iw: f64, ih: f64) -> (f64, f64) {
    let (short, long) = (spec.gutter_short * MM, spec.gutter_long * MM);
    if iw >= ih { (short, long) } else { (long, short) }
}

/// How many fit along a length: n items of size `item` with `gap` between.
fn fit(avail: f64, item: f64, gap: f64) -> u32 {
    if item <= 0.0 || avail < item {
        return 0;
    }
    ((avail + gap) / (item + gap)).floor().max(0.0) as u32
}

/// Best grid for an item on the sheet: tries both sheet orientations and,
/// when allowed, the item turned 90°. Returns (cols, rows, rotated, sheet w, sheet h) in pt.
fn best_grid(spec: &ImposeSpec, item: (f64, f64)) -> (u32, u32, bool, f64, f64) {
    let (sw, sh) = (spec.sheet_w * MM, spec.sheet_h * MM);
    let (m, b) = (spec.margin * MM, spec.bleed * MM);
    // The outer bleed has to fit inside the printable area too.
    let mut best = (0, 0, false, sw, sh);
    let mut best_n = 0;
    for (w, h) in [(sw, sh), (sh, sw)] {
        for rot in [false, true] {
            if rot && !spec.allow_rotate {
                continue;
            }
            let (iw, ih) = if rot { (item.1, item.0) } else { item };
            let (hg, vg) = gaps(spec, iw, ih);
            let cols = fit(w - 2.0 * m - 2.0 * b, iw, hg);
            let rows = fit(h - 2.0 * m - 2.0 * b, ih, vg);
            let n = cols * rows;
            if n > best_n {
                best_n = n;
                best = (cols, rows, rot, w, h);
            }
        }
    }
    best
}

fn plan_for(spec: &ImposeSpec, pages: &[PageInfo]) -> Result<ImposePlan, String> {
    let first = pages.first().ok_or("The PDF has no pages.")?;
    let item = display_size(first);
    let (cols, rows, rotated, w, h) = best_grid(spec, item);
    let per_sheet = cols * rows;
    if per_sheet == 0 {
        return Err("The page doesn't fit on that sheet with these margins and bleed.".into());
    }
    let n = pages.len() as u32;
    let sheets = match spec.mode.as_str() {
        "repeat" => n,
        _ => n.div_ceil(per_sheet),
    };
    let (iw, ih) = if rotated { (item.1, item.0) } else { item };
    Ok(ImposePlan {
        cols,
        rows,
        per_sheet,
        sheets,
        rotated,
        sheet_w: w / MM,
        sheet_h: h / MM,
        item_w: iw / MM,
        item_h: ih / MM,
        pages: n,
    })
}

/// The layout the dialog's settings would give for this PDF.
pub fn plan(path: &Path, spec: &ImposeSpec) -> Result<ImposePlan, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let pages = doc.get_pages().into_values().map(|id| page_info(&doc, id)).collect::<Result<Vec<_>, _>>()?;
    plan_for(spec, &pages)
}

/// A page as a form XObject (its content and resources), drawn in its own
/// page space.
fn page_form(doc: &mut Document, p: &PageInfo) -> Result<ObjectId, String> {
    let content = doc.get_page_content(p.id);
    let page = doc.get_dictionary(p.id).map_err(|e| format!("Could not read a page: {e}"))?.clone();
    let mut d = Dictionary::new();
    d.set("Type", Object::Name(b"XObject".to_vec()));
    d.set("Subtype", Object::Name(b"Form".to_vec()));
    d.set("BBox", Object::Array(p.media.iter().map(|v| Object::Real(*v as f32)).collect()));
    if let Ok(r) = page.get(b"Resources") {
        d.set("Resources", r.clone());
    }
    // Keep the page's transparency group (blend colour space etc.).
    if let Ok(g) = page.get(b"Group") {
        d.set("Group", g.clone());
    }
    let mut s = Stream::new(d, content);
    let _ = s.compress();
    Ok(doc.add_object(s))
}

fn n(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

/// Writes the imposed PDF to `out` and returns its layout.
pub fn impose(path: &Path, out: &Path, spec: &ImposeSpec) -> Result<ImposePlan, String> {
    let mut doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for id in &ids {
        crate::pages::own_inherited(&mut doc, *id);
    }
    let pages = ids.iter().map(|id| page_info(&doc, *id)).collect::<Result<Vec<_>, _>>()?;
    let plan = plan_for(spec, &pages)?;
    let forms = pages.iter().map(|p| page_form(&mut doc, p)).collect::<Result<Vec<_>, _>>()?;

    let (sw, sh) = (plan.sheet_w * MM, plan.sheet_h * MM);
    let b = spec.bleed * MM;
    let (iw, ih) = (plan.item_w * MM, plan.item_h * MM);
    let (cols, rows) = (plan.cols, plan.rows);
    // The grid of trims, centred on the sheet.
    let (hg, vg) = gaps(spec, iw, ih);
    let grid_w = cols as f64 * iw + (cols - 1) as f64 * hg;
    let grid_h = rows as f64 * ih + (rows - 1) as f64 * vg;
    let gx = (sw - grid_w) / 2.0;
    let gy = (sh - grid_h) / 2.0;
    // Bleed shown between items: no more than half the gap, so neighbours
    // never overlap; full bleed on the outside edges.
    let inner_x = b.min(hg / 2.0);
    let inner_y = b.min(vg / 2.0);

    // Registration colour: prints on every plate (Separation /All).
    let mut func = Dictionary::new();
    func.set("FunctionType", Object::Integer(2));
    func.set("Domain", Object::Array(vec![0.into(), 1.into()]));
    func.set("C0", Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]));
    func.set("C1", Object::Array(vec![1.into(), 1.into(), 1.into(), 1.into()]));
    func.set("N", Object::Integer(1));
    let func_id = doc.add_object(func);
    let all = doc.add_object(Object::Array(vec![
        Object::Name(b"Separation".to_vec()),
        Object::Name(b"All".to_vec()),
        Object::Name(b"DeviceCMYK".to_vec()),
        Object::Reference(func_id),
    ]));

    // Which page goes in each cell of each sheet.
    let per = plan.per_sheet as usize;
    let count = pages.len();
    let cell_page = |sheet: usize, cell: usize| -> Option<usize> {
        match spec.mode.as_str() {
            "repeat" => Some(sheet),
            "cutstack" => {
                let i = cell * plan.sheets as usize + sheet;
                (i < count).then_some(i)
            }
            _ => {
                let i = sheet * per + cell;
                (i < count).then_some(i)
            }
        }
    };

    let mut new_pages = Vec::new();
    for sheet in 0..plan.sheets as usize {
        let mut ops = String::new();
        let mut xobjects = Dictionary::new();
        for row in 0..rows as usize {
            for col in 0..cols as usize {
                // Cells run left to right, top to bottom.
                let cell = row * cols as usize + col;
                let Some(pi) = cell_page(sheet, cell) else { continue };
                let p = &pages[pi];
                let x = gx + col as f64 * (iw + hg);
                let y = gy + (rows as usize - 1 - row) as f64 * (ih + vg);
                // Page → display → (turned) → cell.
                let mut m = display_matrix(p);
                if plan.rotated {
                    // Display size before turning is (ih, iw); turn 90° anticlockwise.
                    m = mul(m, [0.0, 1.0, -1.0, 0.0, iw, 0.0]);
                }
                m = mul(m, [1.0, 0.0, 0.0, 1.0, x, y]);
                let left = if col == 0 { b } else { inner_x };
                let right = if col + 1 == cols as usize { b } else { inner_x };
                let bottom = if row + 1 == rows as usize { b } else { inner_y };
                let top = if row == 0 { b } else { inner_y };
                let name = format!("P{pi}");
                xobjects.set(name.as_bytes().to_vec(), Object::Reference(forms[pi]));
                ops.push_str(&format!(
                    "q {} {} {} {} re W n {} {} {} {} {} {} cm /{name} Do Q\n",
                    n(x - left),
                    n(y - bottom),
                    n(iw + left + right),
                    n(ih + top + bottom),
                    n(m[0]),
                    n(m[1]),
                    n(m[2]),
                    n(m[3]),
                    n(m[4]),
                    n(m[5]),
                ));
            }
        }
        if spec.crop_marks {
            // Marks on the outside of the grid, in line with every trim edge,
            // starting just outside the bleed.
            let off = b + 1.0 * MM;
            let len = 5.0 * MM;
            ops.push_str("q /All CS 1 SCN 0.25 w\n");
            let mut xs = Vec::new();
            for c in 0..cols as usize {
                let x = gx + c as f64 * (iw + hg);
                xs.push(x);
                xs.push(x + iw);
            }
            let mut ys = Vec::new();
            for r in 0..rows as usize {
                let y = gy + r as f64 * (ih + vg);
                ys.push(y);
                ys.push(y + ih);
            }
            for x in &xs {
                ops.push_str(&format!("{} {} m {} {} l S\n", n(*x), n(gy - off), n(*x), n(gy - off - len)));
                ops.push_str(&format!("{} {} m {} {} l S\n", n(*x), n(gy + grid_h + off), n(*x), n(gy + grid_h + off + len)));
            }
            for y in &ys {
                ops.push_str(&format!("{} {} m {} {} l S\n", n(gx - off), n(*y), n(gx - off - len), n(*y)));
                ops.push_str(&format!("{} {} m {} {} l S\n", n(gx + grid_w + off), n(*y), n(gx + grid_w + off + len), n(*y)));
            }
            ops.push_str("Q\n");
        }
        let mut res = Dictionary::new();
        res.set("XObject", Object::Dictionary(xobjects));
        let mut cs = Dictionary::new();
        cs.set("All", Object::Reference(all));
        res.set("ColorSpace", Object::Dictionary(cs));
        let mut content = Stream::new(Dictionary::new(), ops.into_bytes());
        let _ = content.compress();
        let content_id = doc.add_object(content);
        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set("MediaBox", Object::Array(vec![0.into(), 0.into(), Object::Real(sw as f32), Object::Real(sh as f32)]));
        page.set("Resources", Object::Dictionary(res));
        page.set("Contents", Object::Reference(content_id));
        new_pages.push(doc.add_object(page));
    }

    // A new page tree of the sheets replaces the old one; the old pages
    // (and what only they used) are dropped when saving.
    let mut tree = Dictionary::new();
    tree.set("Type", Object::Name(b"Pages".to_vec()));
    tree.set("Kids", Object::Array(new_pages.iter().map(|id| Object::Reference(*id)).collect()));
    tree.set("Count", Object::Integer(new_pages.len() as i64));
    let tree_id = doc.add_object(tree);
    for id in &new_pages {
        doc.get_dictionary_mut(*id).map_err(|e| e.to_string())?.set("Parent", Object::Reference(tree_id));
    }
    let root = doc.trailer.get(b"Root").and_then(|o| o.as_reference()).map_err(|_| "The PDF has no catalog.".to_string())?;
    let catalog = doc.get_dictionary_mut(root).map_err(|_| "The PDF has no catalog.".to_string())?;
    catalog.set("Pages", Object::Reference(tree_id));
    // These point at the old pages.
    for key in [b"Outlines".as_slice(), b"StructTreeRoot", b"MarkInfo", b"PageLabels", b"OpenAction", b"Dests", b"AcroForm", b"PageMode"] {
        catalog.remove(key);
    }
    doc.prune_objects();
    doc.save(out).map_err(|e| format!("Could not save the imposed PDF: {e}"))?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: &str) -> ImposeSpec {
        ImposeSpec { mode: mode.into(), sheet_w: 320.0, sheet_h: 450.0, margin: 5.0, gutter_short: 6.0, gutter_long: 6.0, bleed: 3.0, allow_rotate: true, crop_marks: true }
    }

    #[test]
    fn business_cards_on_sra3() {
        // 85 × 55 mm card.
        let p = PageInfo { id: (1, 0), media: [0.0; 4], trim: [0.0, 0.0, 85.0 * MM, 55.0 * MM], rotate: 0 };
        let plan = plan_for(&spec("repeat"), &[p]).unwrap();
        println!("{plan:?}");
        assert!(plan.per_sheet >= 20, "{plan:?}");
    }

    #[test]
    fn matrices() {
        // A point at the trim's top-left of a 90°-rotated page lands at the
        // display's top-left.
        let p = PageInfo { id: (1, 0), media: [0.0, 0.0, 100.0, 50.0], trim: [0.0, 0.0, 100.0, 50.0], rotate: 90 };
        let m = display_matrix(&p);
        let apply = |x: f64, y: f64| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        // Page bottom-left (0,0) shows at the display's top-left (0, 100).
        assert_eq!(apply(0.0, 0.0), (0.0, 100.0));
        assert_eq!(apply(100.0, 50.0), (50.0, 0.0));
    }

    /// `MP_IMPOSE_PDF=in.pdf MP_IMPOSE_OUT=out.pdf cargo test impose_file -- --nocapture`
    #[test]
    fn impose_file() {
        let (Ok(i), Ok(o)) = (std::env::var("MP_IMPOSE_PDF"), std::env::var("MP_IMPOSE_OUT")) else { return };
        for mode in ["repeat", "nup", "cutstack"] {
            let out = format!("{o}.{mode}.pdf");
            let plan = impose(Path::new(&i), Path::new(&out), &spec(mode)).unwrap();
            println!("{mode}: {plan:?}");
        }
    }
}
