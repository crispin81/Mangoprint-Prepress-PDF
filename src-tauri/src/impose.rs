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
//! - "booklet": saddle-stitched booklet: pages in printer spreads (last and
//!   first on the outside sheet…), padded with blank pages to a multiple
//!   of 4, each spread repeated as many times as fits.
//!
//! Sides: single-sided, every imposed page is a front. Double-sided, the
//! PDF's pages are front/back pairs (1 and 2, 3 and 4…) and the imposed
//! pages alternate front and back; on a back the items sit where their
//! fronts will be once the sheet is turned (positions mirrored as the
//! shift is). Booklets are always double-sided.

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MM: f64 = 72.0 / 25.4;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImposeSpec {
    /// "repeat", "nup", "cutstack" or "booklet".
    pub mode: String,
    /// "single" or "double" (the default) sided.
    #[serde(default)]
    pub sides: String,
    /// Press sheet, mm (either orientation; the better one is used).
    pub sheet_w: f64,
    pub sheet_h: f64,
    /// Space the press can't print on, every edge, mm.
    pub margin: f64,
    /// Gap between items going across the sheet's short edge (between
    /// the columns of a portrait sheet), mm.
    pub gutter_short: f64,
    /// Gap between items going along the sheet's long edge, mm.
    pub gutter_long: f64,
    /// "auto" (whichever fits more), "portrait" or "landscape".
    #[serde(default)]
    pub orientation: String,
    /// Bleed kept around each item, mm.
    pub bleed: f64,
    /// Turn items 90° when more fit that way.
    pub allow_rotate: bool,
    /// A set grid instead of as many as fit: columns and/or rows (0 or
    /// missing = as many as fit that way).
    /// Booklet creep, mm: how far the innermost sheet's pages move in
    /// towards the spine (the outside sheet none, the ones between in
    /// proportion), so the fore-edge margins match once the folded
    /// booklet is trimmed.
    #[serde(default)]
    pub creep: f64,
    #[serde(default)]
    pub cols: u32,
    #[serde(default)]
    pub rows: u32,
    pub crop_marks: bool,
    /// Image shift: moves the whole group (items and marks), mm; +x is
    /// right, +y is up.
    #[serde(default)]
    pub shift_x: f64,
    #[serde(default)]
    pub shift_y: f64,
    /// Sheets come in front/back pairs (1 and 2, 3 and 4…). On a back the
    /// shift is mirrored so it lines up with its front once turned:
    /// left/right (work and turn) and/or up/down (work and tumble).
    #[serde(default)]
    pub mirror_x: bool,
    #[serde(default)]
    pub mirror_y: bool,
    /// When items are turned 90° to fit, turn the artwork on back sheets a
    /// further 180° in its place, so it reads the right way up once the
    /// sheet is turned over. Items placed upright are left alone.
    #[serde(default)]
    pub rotate_backs: bool,
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
    /// For each imposed page, each cell's pages (1-based; two for a
    /// booklet spread, left then right; 0 is a blank page). Empty: no item.
    pub cells: Vec<Vec<Vec<u32>>>,
    /// Whether each imposed page is a back (sheetwise).
    pub backs: Vec<bool>,
    /// Whether each cell of each imposed page holds back artwork (turned
    /// 180° when items are turned and "rotate backs" is on).
    pub flips: Vec<Vec<bool>>,
    /// Sheetwise double-sided: imposed pages alternate front and back.
    pub double: bool,
    /// Work and turn: fronts and backs on one imposed page.
    pub work_turn: bool,
    /// Blank pages added to make a booklet up to a multiple of 4.
    pub blanks: u32,
}

impl ImposeSpec {
    fn booklet(&self) -> bool {
        self.mode == "booklet"
    }
    /// Work and turn: each item's front and back on the same side of the
    /// sheet (fronts on the left half, backs mirrored on the right), the
    /// same plate printed on both sides, turned over left to right.
    fn work_turn(&self) -> bool {
        self.sides == "workturn"
    }
    /// Sheetwise double-sided: fronts and backs on alternate imposed pages.
    fn double(&self) -> bool {
        !self.work_turn() && (self.booklet() || self.sides != "single")
    }
}

/// What `assign` lays out: per imposed page, each cell's pages; whether
/// each page is a back; whether each cell holds back artwork; blanks added.
type Assigned = (Vec<Vec<Vec<u32>>>, Vec<bool>, Vec<Vec<bool>>, u32);

/// Which pages go in each cell of each imposed page (see `ImposePlan`).
fn assign(spec: &ImposeSpec, n: usize, cols: usize, rows: usize) -> Assigned {
    let per = cols * rows;
    let pg = |i: usize| -> Vec<u32> { if i < n { vec![i as u32 + 1] } else { Vec::new() } };
    let wt = spec.work_turn();
    // Where an item in front cell `c` sits on its back. A sheet turns over
    // one way: left/right (work and turn, and always for a work and turn
    // layout) when that's ticked, else top to bottom (tumble).
    let mirror = |c: usize| -> usize {
        let (r, col) = (c / cols, c % cols);
        if wt || spec.mirror_x {
            r * cols + (cols - 1 - col)
        } else if spec.mirror_y {
            (rows - 1 - r) * cols + col
        } else {
            c
        }
    };
    // The cells that take fronts: the left half for work and turn.
    let front_cells: Vec<usize> = (0..per).filter(|c| !wt || c % cols < cols / 2).collect();

    // Units: an item's front and (when double-sided) its back.
    let mut blanks = 0;
    let units: Vec<(Vec<u32>, Option<Vec<u32>>)> = if spec.booklet() {
        let total = n.div_ceil(4).max(1) * 4;
        blanks = (total - n) as u32;
        let num = |i: usize| if i < n { i as u32 + 1 } else { 0 };
        (0..total / 4).map(|i| (vec![num(total - 1 - 2 * i), num(2 * i)], Some(vec![num(2 * i + 1), num(total - 2 - 2 * i)]))).collect()
    } else if spec.double() || wt {
        (0..n.div_ceil(2)).map(|i| (pg(2 * i), Some(pg(2 * i + 1)))).collect()
    } else {
        (0..n).map(|i| (pg(i), None)).collect()
    };

    let mut cells = Vec::new();
    let mut backs = Vec::new();
    let mut flips = Vec::new();
    let mut push = |page: Vec<Vec<u32>>, back: bool, flip: Vec<bool>| {
        cells.push(page);
        backs.push(back);
        flips.push(flip);
    };
    // Every unit placed in `slots` (front cell → unit index), as one or two
    // imposed pages.
    let mut emit = |slot_unit: &dyn Fn(usize) -> Option<usize>| {
        let mut front = vec![Vec::new(); per];
        let mut back = vec![Vec::new(); per];
        let mut flip = vec![false; per];
        let mut any_back = false;
        for (s, &c) in front_cells.iter().enumerate() {
            let Some(u) = slot_unit(s) else { continue };
            let (f, b) = &units[u];
            front[c] = f.clone();
            if let Some(b) = b {
                if !b.is_empty() {
                    any_back = true;
                    if wt {
                        front[mirror(c)] = b.clone();
                        flip[mirror(c)] = true;
                    } else {
                        back[mirror(c)] = b.clone();
                    }
                }
            }
        }
        if wt {
            push(front, false, flip);
        } else {
            push(front, false, vec![false; per]);
            if any_back || spec.booklet() {
                push(back, true, vec![true; per]);
            }
        }
    };
    let fill = spec.mode == "repeat" || spec.booklet();
    if fill {
        // One unit fills the sheet.
        for u in 0..units.len() {
            emit(&|_| Some(u));
        }
    } else {
        let per_f = front_cells.len().max(1);
        let sheets = units.len().div_ceil(per_f);
        for f in 0..sheets {
            emit(&|s| {
                let u = if spec.mode == "cutstack" { s * sheets + f } else { f * per_f + s };
                (u < units.len()).then_some(u)
            });
        }
    }
    (cells, backs, flips, blanks)
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

/// The gaps (pt) between columns and between rows on a `w` × `h` sheet:
/// the short-edge gap runs across the sheet's short side, so it's the
/// column gap on a portrait sheet and the row gap on a landscape one.
fn gaps(spec: &ImposeSpec, w: f64, h: f64) -> (f64, f64) {
    let (short, long) = (spec.gutter_short * MM, spec.gutter_long * MM);
    if w <= h { (short, long) } else { (long, short) }
}

/// How many fit along a length: n items of size `item` with `gap` between.
fn fit(avail: f64, item: f64, gap: f64) -> u32 {
    if item <= 0.0 || avail < item {
        return 0;
    }
    ((avail + gap) / (item + gap)).floor().max(0.0) as u32
}

/// Best grid for an item on the sheet: tries the allowed sheet orientations and,
/// when allowed, the item turned 90°. Returns (cols, rows, rotated, sheet w, sheet h) in pt.
fn best_grid(spec: &ImposeSpec, item: (f64, f64)) -> (u32, u32, bool, f64, f64) {
    let (sw, sh) = (spec.sheet_w * MM, spec.sheet_h * MM);
    let (m, b) = (spec.margin * MM, spec.bleed * MM);
    // The outer bleed has to fit inside the printable area too.
    let (short, long) = (sw.min(sh), sw.max(sh));
    // Work and turn: the sheet turns over end to end along its long
    // (gripper) edge, so fronts and backs sit either side of the middle of
    // the long edge: a landscape sheet, front | back.
    let orientation = if spec.work_turn() { "landscape" } else { spec.orientation.as_str() };
    let sheets: Vec<(f64, f64)> = match orientation {
        "portrait" => vec![(short, long)],
        "landscape" => vec![(long, short)],
        _ => vec![(short, long), (long, short)],
    };
    let mut best = (0, 0, false, sheets[0].0, sheets[0].1);
    let mut best_n = 0;
    for (w, h) in sheets {
        for rot in [false, true] {
            if rot && !spec.allow_rotate {
                continue;
            }
            let (iw, ih) = if rot { (item.1, item.0) } else { item };
            let (hg, vg) = gaps(spec, w, h);
            let max_c = fit(w - 2.0 * m - 2.0 * b, iw, hg);
            let max_r = fit(h - 2.0 * m - 2.0 * b, ih, vg);
            // A set column/row count has to fit; unset ones fill the sheet.
            let cols = if spec.cols > 0 { spec.cols } else { max_c };
            let rows = if spec.rows > 0 { spec.rows } else { max_r };
            if cols > max_c || rows > max_r {
                continue;
            }
            // Work and turn uses columns in pairs (front | back).
            let n = if spec.work_turn() { cols / 2 * 2 * rows } else { cols * rows };
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
    let (pw, ph) = display_size(first);
    // A booklet places printer spreads: two pages side by side.
    let item = if spec.booklet() { (2.0 * pw, ph) } else { (pw, ph) };
    let (cols, rows, rotated, w, h) = best_grid(spec, item);
    let per_sheet = cols * rows;
    if spec.work_turn() && cols < 2 && per_sheet > 0 {
        return Err("Work and turn needs at least two items across the sheet (front and back side by side).".into());
    }
    if per_sheet == 0 {
        if spec.cols > 0 || spec.rows > 0 {
            let want = |v: u32| if v > 0 { v.to_string() } else { "auto".into() };
            return Err(format!(
                "{} columns × {} rows doesn't fit on that sheet with these gaps, margins and bleed.",
                want(spec.cols),
                want(spec.rows)
            ));
        }
        return Err("The page doesn't fit on that sheet with these margins and bleed.".into());
    }
    let n = pages.len() as u32;
    let (cells, backs, flips, blanks) = assign(spec, n as usize, cols as usize, rows as usize);
    let sheets = cells.len() as u32;
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
        cells,
        backs,
        flips,
        double: spec.double(),
        work_turn: spec.work_turn(),
        blanks,
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
    // No transparency group on the form: viewers draw every grouped form
    // into its own offscreen image, which made sheets of 20+ copies slow.
    // The page's group (blending colour space) goes on the sheet instead.
    let mut s = Stream::new(d, content);
    let _ = s.compress();
    Ok(doc.add_object(s))
}

/// A matrix as `cm` operands.
fn cm(m: M) -> String {
    m.iter().map(|v| n(*v)).collect::<Vec<_>>().join(" ")
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
    // The first page's transparency group, for every sheet.
    let sheet_group = doc.get_dictionary(pages[0].id).ok().and_then(|d| d.get(b"Group").ok()).cloned();

    let (sw, sh) = (plan.sheet_w * MM, plan.sheet_h * MM);
    let b = spec.bleed * MM;
    let (iw, ih) = (plan.item_w * MM, plan.item_h * MM);
    let (cols, rows) = (plan.cols, plan.rows);
    // The grid of trims, centred on the sheet.
    let (hg, vg) = gaps(spec, sw, sh);
    let grid_w = cols as f64 * iw + (cols - 1) as f64 * hg;
    let grid_h = rows as f64 * ih + (rows - 1) as f64 * vg;
    // Centred; the image shift is added per sheet (mirrored on backs).
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

    let booklet = spec.booklet();
    // A booklet page's width within its spread (before any turn), pt.
    let page_w = if plan.rotated { ih / 2.0 } else { iw / 2.0 };
    // Booklet sheets of paper (2 imposed pages each), outside first.
    let paper = if plan.work_turn { plan.sheets as usize } else { (plan.sheets as usize).div_ceil(2) }.max(1);
    let creep_at = |k: usize| -> f64 {
        if !booklet || paper < 2 || spec.creep <= 0.0 {
            return 0.0;
        }
        let i = if plan.work_turn { k } else { k / 2 };
        spec.creep.min(20.0) * MM * i as f64 / (paper - 1) as f64
    };

    let mut new_pages = Vec::new();
    for sheet in 0..plan.sheets as usize {
        let back = plan.backs[sheet];
        let sx = if back && spec.mirror_x { -spec.shift_x } else { spec.shift_x };
        let sy = if back && spec.mirror_y { -spec.shift_y } else { spec.shift_y };
        let gx = (sw - grid_w) / 2.0 + sx * MM;
        let gy = (sh - grid_h) / 2.0 + sy * MM;
        let mut ops = String::new();
        let mut xobjects = Dictionary::new();
        for row in 0..rows as usize {
            for col in 0..cols as usize {
                // Cells run left to right, top to bottom.
                let cell = row * cols as usize + col;
                let slot = &plan.cells[sheet][cell];
                if slot.is_empty() || slot.iter().all(|&p| p == 0) {
                    continue;
                }
                let x = gx + col as f64 * (iw + hg);
                let y = gy + (rows as usize - 1 - row) as f64 * (ih + vg);
                // Item (page, or booklet spread) → (turned) → cell.
                let mut m: M = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
                if plan.rotated {
                    // Display size before turning is (ih, iw); turn 90° anticlockwise.
                    m = mul(m, [0.0, 1.0, -1.0, 0.0, iw, 0.0]);
                }
                if plan.flips[sheet][cell] && spec.rotate_backs && plan.rotated {
                    m = mul(m, [-1.0, 0.0, 0.0, -1.0, iw, ih]);
                }
                m = mul(m, [1.0, 0.0, 0.0, 1.0, x, y]);
                let place = m;
                let left = if col == 0 { b } else { inner_x };
                let right = if col + 1 == cols as usize { b } else { inner_x };
                let bottom = if row + 1 == rows as usize { b } else { inner_y };
                let top = if row == 0 { b } else { inner_y };
                // The cell with its bleed, in sheet space.
                ops.push_str(&format!("q {} {} {} {} re W n {} cm\n", n(x - left), n(y - bottom), n(iw + left + right), n(ih + top + bottom), cm(place)));
                for (j, &pnum) in slot.iter().enumerate() {
                    if pnum == 0 {
                        continue; // blank booklet page
                    }
                    let pi = pnum as usize - 1;
                    let p = &pages[pi];
                    let name = format!("P{pi}");
                    xobjects.set(name.as_bytes().to_vec(), Object::Reference(forms[pi]));
                    let mut pm = display_matrix(p);
                    if booklet {
                        // Left or right half of the spread; no bleed across the spine.
                        let (x0, x1) = if j == 0 { (-1.0e4, page_w) } else { (page_w, 1.0e4) };
                        ops.push_str(&format!("q {} -10000 {} 20000 re W n ", n(x0), n(x1 - x0)));
                        // Creep: both pages move in towards the spine.
                        let c = creep_at(sheet);
                        let dx = if j == 0 { c } else { -c };
                        pm = mul(pm, [1.0, 0.0, 0.0, 1.0, j as f64 * page_w + dx, 0.0]);
                    } else {
                        ops.push_str("q ");
                    }
                    ops.push_str(&format!("{} cm /{name} Do Q\n", cm(pm)));
                }
                ops.push_str("Q\n");
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
                if booklet && !plan.rotated {
                    xs.push(x + iw / 2.0); // fold
                }
            }
            let mut ys = Vec::new();
            for r in 0..rows as usize {
                let y = gy + r as f64 * (ih + vg);
                ys.push(y);
                ys.push(y + ih);
                if booklet && plan.rotated {
                    ys.push(y + ih / 2.0); // fold
                }
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
        // TrimBox: the outermost trim lines of the grid on this sheet (with
        // its image shift), i.e. where the outer crop marks point.
        page.set(
            "TrimBox",
            Object::Array(vec![Object::Real(gx as f32), Object::Real(gy as f32), Object::Real((gx + grid_w) as f32), Object::Real((gy + grid_h) as f32)]),
        );
        page.set("Resources", Object::Dictionary(res));
        if let Some(g) = &sheet_group {
            page.set("Group", g.clone());
        }
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
        ImposeSpec { mode: mode.into(), sides: "double".into(), sheet_w: 320.0, sheet_h: 450.0, margin: 5.0, gutter_short: 6.0, gutter_long: 6.0, orientation: "auto".into(), bleed: 3.0, allow_rotate: true, creep: 0.0, cols: 0, rows: 0, crop_marks: true, shift_x: 0.0, shift_y: 0.0, mirror_x: true, mirror_y: false, rotate_backs: false }
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
    fn booklet_order() {
        // 6 pages → 8 (2 blanks): outside sheet 8|1 and 2|7, inside 6|3 and 4|5.
        let mut sp = spec("booklet");
        sp.allow_rotate = false;
        let (cells, backs, _, blanks) = assign(&sp, 6, 1, 1);
        assert_eq!(blanks, 2);
        let firsts: Vec<Vec<u32>> = cells.iter().map(|c| c[0].clone()).collect();
        assert_eq!(firsts, vec![vec![0, 1], vec![2, 0], vec![6, 3], vec![4, 5]]);
        assert_eq!(backs, vec![false, true, false, true]);
    }

    #[test]
    fn double_sided_unique_backs_mirror() {
        // 3 cols × 1 row, 6 pages = 3 items: fronts 1 3 5, backs mirrored left/right: 6 4 2.
        let mut sp = spec("nup");
        sp.mirror_x = true;
        sp.mirror_y = false;
        let (cells, backs, _, _) = assign(&sp, 6, 3, 1);
        assert_eq!(cells[0], vec![vec![1], vec![3], vec![5]]);
        assert_eq!(cells[1], vec![vec![6], vec![4], vec![2]]);
        assert_eq!(backs, vec![false, true]);
        sp.sides = "single".into();
        let (cells, backs, _, _) = assign(&sp, 6, 3, 1);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[1], vec![vec![4], vec![5], vec![6]]);
        assert_eq!(backs, vec![false, false]);
    }

    #[test]
    fn both_mirrors_turn_left_right() {
        // 2 × 2, both boxes ticked: backs swap columns only.
        let mut sp = spec("nup");
        sp.mirror_x = true;
        sp.mirror_y = true;
        let (cells, _, _, _) = assign(&sp, 8, 2, 2);
        assert_eq!(cells[1], vec![vec![4], vec![2], vec![8], vec![6]]);
        sp.mirror_x = false;
        let (cells, _, _, _) = assign(&sp, 8, 2, 2);
        assert_eq!(cells[1], vec![vec![6], vec![8], vec![2], vec![4]]);
    }

    #[test]
    fn work_and_turn_booklet() {
        // 8 pages, 2 spreads across: each sheet has its front | back spread.
        let mut sp = spec("booklet");
        sp.sides = "workturn".into();
        let (cells, backs, flips, _) = assign(&sp, 8, 2, 1);
        assert_eq!(cells, vec![vec![vec![8, 1], vec![2, 7]], vec![vec![6, 3], vec![4, 5]]]);
        assert_eq!(backs, vec![false, false]);
        assert_eq!(flips[0], vec![false, true]);
        // Cards: 4 pages = 2 items, 4 across: fronts 1 3 | backs 4 2.
        let mut sp = spec("nup");
        sp.sides = "workturn".into();
        let (cells, _, _, _) = assign(&sp, 4, 4, 1);
        assert_eq!(cells, vec![vec![vec![1], vec![3], vec![4], vec![2]]]);
    }

    #[test]
    fn work_and_turn_splits_the_long_edge() {
        // A5 booklet (spread 297 × 210) on SRA2: landscape 640 × 450, front | back.
        let p = PageInfo { id: (1, 0), media: [0.0; 4], trim: [0.0, 0.0, 148.5 * MM, 210.0 * MM], rotate: 0 };
        let mut sp = spec("booklet");
        sp.sides = "workturn".into();
        sp.orientation = "portrait".into(); // overridden
        (sp.sheet_w, sp.sheet_h) = (450.0, 640.0);
        let plan = plan_for(&sp, &[p]).unwrap();
        assert!(plan.sheet_w > plan.sheet_h, "{plan:?}");
        assert_eq!(plan.cols % 2, 0);
    }

    #[test]
    fn set_grid() {
        let p = || PageInfo { id: (1, 0), media: [0.0; 4], trim: [0.0, 0.0, 85.0 * MM, 55.0 * MM], rotate: 0 };
        let mut sp = spec("repeat");
        sp.cols = 2;
        sp.rows = 3;
        let plan = plan_for(&sp, &[p()]).unwrap();
        assert_eq!((plan.cols, plan.rows), (2, 3));
        sp.rows = 0; // as many rows as fit
        let plan = plan_for(&sp, &[p()]).unwrap();
        assert_eq!(plan.cols, 2);
        assert!(plan.rows > 3);
        sp.cols = 40;
        assert!(plan_for(&sp, &[p()]).is_err());
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
        for mode in ["repeat", "nup", "cutstack", "repeat-rotated-backs", "booklet", "booklet-creep", "booklet-workturn-backs"] {
            let out = format!("{o}.{mode}.pdf");
            let mut sp = spec(mode.split('-').next().unwrap());
            if mode == "booklet-creep" {
                sp.creep = 10.0;
            }
            if mode.starts_with("booklet-workturn") {
                sp.sides = "workturn".into();
                (sp.sheet_w, sp.sheet_h) = (450.0, 640.0);
            }
            sp.rotate_backs = mode.ends_with("backs");
            let plan = impose(Path::new(&i), Path::new(&out), &sp).unwrap();
            println!("{mode}: {plan:?}");
        }
    }
}

#[cfg(test)]
mod trim_tests {
    use super::*;

    /// Each sheet's TrimBox is the grid's outer trim lines.
    #[test]
    fn sheet_trim_box() {
        let Ok(i) = std::env::var("MP_IMPOSE_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("x.pdf");
        let spec = ImposeSpec { mode: "repeat".into(), sides: "double".into(), sheet_w: 320.0, sheet_h: 450.0, margin: 5.0, gutter_short: 6.0, gutter_long: 6.0, orientation: "portrait".into(), bleed: 3.0, allow_rotate: false, creep: 0.0, cols: 0, rows: 0, crop_marks: true, shift_x: 0.0, shift_y: 0.0, mirror_x: true, mirror_y: true, rotate_backs: true };
        let plan = impose(Path::new(&i), &out, &spec).unwrap();
        let doc = Document::load(&out).unwrap();
        let page = doc.get_dictionary(*doc.get_pages().get(&1).unwrap()).unwrap();
        let t = rect(&doc, page, b"TrimBox").unwrap();
        let w_mm = (t[2] - t[0]) / MM;
        let expect = plan.cols as f64 * plan.item_w + (plan.cols - 1) as f64 * 6.0;
        println!("trim {t:?} width {w_mm} mm, expected {expect}");
        assert!((w_mm - expect).abs() < 0.1);
        assert!(((t[0] + t[2]) / 2.0 - 160.0 * MM).abs() < 0.5, "centred");
    }
}
