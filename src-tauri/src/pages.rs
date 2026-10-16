//! Page-level edits: inserting the pages of other PDFs, deleting a page,
//! and mail merge (one copy of a page per CSV row, each with that row's
//! text added in a box).

use crate::gs;
use crate::pagebox;
use crate::recolour::page_geom;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Deserialize;
use std::path::Path;

fn err(e: lopdf::Error) -> String {
    format!("Could not update the page tree: {e}")
}

fn load(path: &Path) -> Result<Document, String> {
    Document::load(path).map_err(|e| format!("Could not open {}: {e}", path.display()))
}

/// Saves, first dropping objects nothing refers to any more.
fn save(mut doc: Document, path: &Path) -> Result<(), String> {
    doc.prune_objects();
    doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;
    Ok(())
}

/// Page attributes a page can inherit from the page tree above it.
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// Copies inherited attributes onto the page itself, so it keeps them when
/// it moves to a different page tree (or is copied).
fn own_inherited(doc: &mut Document, page_id: ObjectId) {
    for key in INHERITABLE {
        if doc.get_dictionary(page_id).map(|d| d.has(key)).unwrap_or(true) {
            continue;
        }
        let mut id = page_id;
        let mut found = None;
        while let Ok(Object::Reference(pid)) = doc.get_dictionary(id).and_then(|d| d.get(b"Parent")).cloned() {
            id = pid;
            if let Ok(v) = doc.get_dictionary(id).and_then(|d| d.get(key)) {
                found = Some(v.clone());
                break;
            }
        }
        if let (Some(v), Ok(d)) = (found, doc.get_dictionary_mut(page_id)) {
            d.set(key.to_vec(), v);
        }
    }
}

/// Moves every object of `src` into `dest` (renumbered so nothing clashes),
/// except its catalog and page tree. Returns `src`'s pages in order; they
/// still have to be put into `dest`'s page tree.
fn import(dest: &mut Document, mut src: Document) -> Vec<ObjectId> {
    let ids: Vec<ObjectId> = src.get_pages().into_values().collect();
    for id in ids {
        own_inherited(&mut src, id);
    }
    src.renumber_objects_with(dest.max_id + 1);
    let pages: Vec<ObjectId> = src.get_pages().into_values().collect();
    let catalog = src.trailer.get(b"Root").and_then(|o| o.as_reference()).ok();
    for (id, obj) in std::mem::take(&mut src.objects) {
        let is_tree = matches!(&obj, Object::Dictionary(d) if d.get(b"Type").and_then(|t| t.as_name()).ok() == Some(b"Pages".as_slice()));
        if Some(id) == catalog || is_tree {
            continue;
        }
        dest.objects.insert(id, obj);
    }
    dest.max_id = dest.objects.keys().map(|k| k.0).max().unwrap_or(dest.max_id);
    pages
}

/// Puts `new` pages into the page tree so that `at` pages come before them.
fn insert_into_tree(doc: &mut Document, at: u32, new: &[ObjectId]) -> Result<(), String> {
    let pages = doc.get_pages();
    let count = pages.len() as u32;
    if count == 0 {
        return Err("The PDF has no pages to insert next to.".into());
    }
    let (anchor, after) = if at < count { (pages[&(at + 1)], false) } else { (pages[&count], true) };
    let parent = doc
        .get_dictionary(anchor)
        .and_then(|d| d.get(b"Parent"))
        .and_then(|p| p.as_reference())
        .map_err(err)?;
    let kids_ref = match doc.get_dictionary(parent).and_then(|d| d.get(b"Kids")).map_err(err)? {
        Object::Reference(k) => Some(*k),
        _ => None,
    };
    let kids = match kids_ref {
        Some(k) => doc.get_object_mut(k).and_then(Object::as_array_mut).map_err(err)?,
        None => doc
            .get_dictionary_mut(parent)
            .and_then(|d| d.get_mut(b"Kids"))
            .and_then(Object::as_array_mut)
            .map_err(err)?,
    };
    let pos = kids
        .iter()
        .position(|o| o.as_reference().ok() == Some(anchor))
        .ok_or("The PDF's page tree is damaged.")?;
    let pos = if after { pos + 1 } else { pos };
    for (i, id) in new.iter().enumerate() {
        kids.insert(pos + i, Object::Reference(*id));
    }
    for id in new {
        doc.get_dictionary_mut(*id).map_err(err)?.set("Parent", Object::Reference(parent));
    }
    let mut node = Some(parent);
    while let Some(n) = node {
        let d = doc.get_dictionary_mut(n).map_err(err)?;
        let c = d.get(b"Count").and_then(|c| c.as_i64()).unwrap_or(0);
        d.set("Count", c + new.len() as i64);
        node = d.get(b"Parent").and_then(|p| p.as_reference()).ok();
    }
    Ok(())
}

/// Inserts every page of each PDF in `sources` (in order) so that `at`
/// pages of the document come before them. Returns the new page count.
pub fn insert_pdfs(path: &Path, sources: &[String], at: u32) -> Result<u32, String> {
    let mut doc = load(path)?;
    let mut new = Vec::new();
    for s in sources {
        let src = load(Path::new(s))?;
        new.extend(import(&mut doc, src));
    }
    if new.is_empty() {
        return Err("Those PDFs have no pages to insert.".into());
    }
    let at = at.min(doc.get_pages().len() as u32);
    insert_into_tree(&mut doc, at, &new)?;
    let count = doc.get_pages().len() as u32;
    save(doc, path)?;
    Ok(count)
}

/// Moves page `from` so it becomes page `to` (both 1-based).
pub fn move_page(path: &Path, from: u32, to: u32) -> Result<(), String> {
    let mut doc = load(path)?;
    let count = doc.get_pages().len() as u32;
    if from == to || from < 1 || to < 1 || from > count || to > count {
        return Ok(());
    }
    let id = pagebox::page_id_for(&doc, from)?;
    own_inherited(&mut doc, id);
    // Take it out of its parent's Kids and out of the counts above it.
    let parent = doc
        .get_dictionary(id)
        .and_then(|d| d.get(b"Parent"))
        .and_then(|p| p.as_reference())
        .map_err(err)?;
    let kids_ref = match doc.get_dictionary(parent).and_then(|d| d.get(b"Kids")).map_err(err)? {
        Object::Reference(k) => Some(*k),
        _ => None,
    };
    let kids = match kids_ref {
        Some(k) => doc.get_object_mut(k).and_then(Object::as_array_mut).map_err(err)?,
        None => doc
            .get_dictionary_mut(parent)
            .and_then(|d| d.get_mut(b"Kids"))
            .and_then(Object::as_array_mut)
            .map_err(err)?,
    };
    kids.retain(|o| o.as_reference().ok() != Some(id));
    let mut node = Some(parent);
    while let Some(n) = node {
        let d = doc.get_dictionary_mut(n).map_err(err)?;
        let c = d.get(b"Count").and_then(|c| c.as_i64()).unwrap_or(1);
        d.set("Count", c - 1);
        node = d.get(b"Parent").and_then(|p| p.as_reference()).ok();
    }
    insert_into_tree(&mut doc, to - 1, &[id])?;
    save(doc, path)
}

/// Deletes one page. Returns the new page count.
pub fn delete_page(path: &Path, page: u32) -> Result<u32, String> {
    delete_pages(path, &[page])
}

/// Deletes several pages (numbers as they are before deleting). Returns
/// the new page count.
pub fn delete_pages(path: &Path, pages: &[u32]) -> Result<u32, String> {
    let mut doc = load(path)?;
    let count = doc.get_pages().len() as u32;
    let mut pages: Vec<u32> = pages.to_vec();
    pages.sort_unstable();
    pages.dedup();
    if pages.len() as u32 >= count {
        return Err("A PDF needs at least one page, so not every page can be deleted.".into());
    }
    for &p in &pages {
        pagebox::page_id_for(&doc, p)?;
    }
    doc.delete_pages(&pages);
    let left = doc.get_pages().len() as u32;
    save(doc, path)?;
    Ok(left)
}

// ---------------------------------------------------------------------------
// Mail merge

fn is_spreadsheet(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "xlsx" | "xlsm" | "xlsb" | "xls" | "ods")
}

/// The names of a spreadsheet's sheets, in order; empty for a CSV file.
pub fn sheet_names(path: &Path) -> Result<Vec<String>, String> {
    if !is_spreadsheet(path) {
        return Ok(Vec::new());
    }
    use calamine::{open_workbook_auto, Reader};
    let book = open_workbook_auto(path).map_err(|e| format!("Could not read the spreadsheet: {e}"))?;
    Ok(book.sheet_names())
}

/// The rows of a CSV file, or of a spreadsheet's sheet (`sheet`, or the
/// first one), each split into its columns. Blank rows are left out.
pub fn read_csv(path: &Path, sheet: Option<&str>) -> Result<Vec<Vec<String>>, String> {
    if is_spreadsheet(path) {
        return read_spreadsheet(path, sheet);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read the CSV file: {e}"))?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    // UTF-8, or (as Excel often saves) a Windows / Latin-1 file.
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    };
    // Comma-separated unless the first line has more semicolons or tabs.
    let first_line = text.lines().next().unwrap_or("");
    let count = |c: char| first_line.matches(c).count();
    let sep = [',', ';', '\t'].into_iter().max_by_key(|c| (count(*c), *c == ',')).unwrap_or(',');
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let end_row = |field: &mut String, row: &mut Vec<String>, rows: &mut Vec<Vec<String>>| {
        row.push(field.trim().to_string());
        field.clear();
        let r = std::mem::take(row);
        if r.iter().any(|v| !v.is_empty()) {
            rows.push(r);
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            c if c == sep => row.push(std::mem::take(&mut field).trim().to_string()),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                end_row(&mut field, &mut row, &mut rows);
            }
            '\n' => end_row(&mut field, &mut row, &mut rows),
            _ => field.push(c),
        }
    }
    end_row(&mut field, &mut row, &mut rows);
    Ok(rows)
}

/// The rows of one sheet (`sheet`, or the first) of an Excel or
/// OpenDocument spreadsheet, as the cells show them. Blank rows are left
/// out.
fn read_spreadsheet(path: &Path, sheet: Option<&str>) -> Result<Vec<Vec<String>>, String> {
    use calamine::{open_workbook_auto, Data, Reader};
    let mut book = open_workbook_auto(path).map_err(|e| format!("Could not read the spreadsheet: {e}"))?;
    let range = match sheet {
        Some(name) => book.worksheet_range(name).map_err(|e| format!("Could not read the sheet \"{name}\": {e}"))?,
        None => book
            .worksheet_range_at(0)
            .ok_or("The spreadsheet has no sheets.")?
            .map_err(|e| format!("Could not read the spreadsheet: {e}"))?,
    };
    let cell = |d: &Data| match d {
        // Whole numbers without ".0", so codes and phone numbers read as typed.
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::Empty | Data::Error(_) => String::new(),
        other => other.to_string(),
    };
    Ok(range
        .rows()
        .map(|r| r.iter().map(|d| cell(d).trim().to_string()).collect::<Vec<_>>())
        .filter(|r| r.iter().any(|v| !v.is_empty()))
        .collect())
}

/// One text box of a mail merge.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MergeField {
    /// "text" (the default), or a barcode: "qr", "datamatrix" or
    /// "gs1datamatrix".
    #[serde(default)]
    pub kind: String,
    /// The box as [left, top, width, height] of the displayed page.
    pub rect: [f64; 4],
    /// "sans", "sans-bold", "serif" or "mono".
    pub font: String,
    /// Point size.
    pub size: f64,
    /// Text colour, CMYK 0–100.
    pub cmyk: [f64; 4],
    /// "left", "centre" or "right".
    pub align: String,
    /// "top", "middle" or "bottom".
    pub valign: String,
    /// Make text that's too wide for the box smaller so it fits.
    pub shrink: bool,
    pub overprint: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MergeSpec {
    pub fields: Vec<MergeField>,
}

/// PostScript hex string of `s` in Latin-1 (characters outside it become `?`).
fn ps_hex(s: &str) -> String {
    let mut out = String::from("<");
    for c in s.chars() {
        let b = match c as u32 {
            0..=31 => b' ',
            n @ 32..=255 => n as u8,
            _ => b'?',
        };
        out.push_str(&format!("{b:02X}"));
    }
    out.push('>');
    out
}

/// Where a field's box sits on the page: its centre in PDF space and its
/// width and height as displayed.
struct Placed<'a> {
    field: &'a MergeField,
    centre: (f64, f64),
    size: (f64, f64),
}

/// The PostScript font name for a field: one of Ghostscript's own fonts,
/// or (`sys:<path>`) an installed font, which is copied into `font_dir`
/// so Ghostscript can find and embed it.
fn font_name(font: &str, font_dir: &Path) -> Result<String, String> {
    if let Some(path) = font.strip_prefix("sys:") {
        let p = Path::new(path);
        let f = crate::sysfonts::read_font(p).ok_or_else(|| format!("The font {path} can't be used for mail merge."))?;
        let dest = font_dir.join(p.file_name().ok_or("Invalid font path")?);
        if !dest.exists() {
            std::fs::copy(p, &dest).map_err(|e| format!("Could not read the font {path}: {e}"))?;
        }
        return Ok(f.ps_name);
    }
    Ok(match font {
        "sans-bold" => "NimbusSans-Bold",
        "serif" => "NimbusRoman-Regular",
        "mono" => "NimbusMonoPS-Regular",
        _ => "NimbusSans-Regular",
    }
    .to_string())
}

/// A PostScript program drawing one page per row, each the size of the
/// template page's MediaBox, with each of the row's values set in its box.
fn merge_program(rows: &[Vec<String>], placed: &[Placed], fonts: &[String], media: [f64; 4], rotate: i32) -> Result<String, String> {
    let f = |v: f64| format!("{v:.4}");
    let mut ps = format!(
        "%!PS\n<< /PageSize [{} {}] >> setpagedevice\n",
        f(media[2] - media[0]),
        f(media[3] - media[1])
    );
    for (j, p) in placed.iter().enumerate() {
        let spec = p.field;
        if is_barcode(spec) {
            continue;
        }
        let font = &fonts[j];
        let (bw, bh) = (f(p.size.0), f(p.size.1));
        let x = match spec.align.as_str() {
            "left" => format!("{bw} -2 div"),
            "right" => format!("{bw} 2 div sw sub"),
            _ => "sw -2 div".to_string(),
        };
        let y = match spec.valign.as_str() {
            "top" => format!("{bh} 2 div fs 0.72 mul sub"),
            "bottom" => format!("{bh} -2 div fs 0.22 mul add"),
            _ => "fs 0.35 mul neg".to_string(),
        };
        let [c, m, ye, k] = spec.cmyk.map(|v| f((v / 100.0).clamp(0.0, 1.0)));
        let size = f(spec.size.max(0.5));
        ps.push_str(&format!(
            "/MPFont{j} /{font} findfont dup length dict begin\n\
             {{ 1 index /FID ne {{ def }} {{ pop pop }} ifelse }} forall\n\
             /Encoding ISOLatin1Encoding def currentdict end definefont pop\n\
             /MPshow{j} {{\n\
               /s exch def /fs {size} def\n\
               /MPFont{j} findfont fs scalefont setfont s stringwidth pop /sw exch def\n\
               {shrink} sw {bw} gt and {{\n\
                 /fs {size} {bw} mul sw div def\n\
                 /MPFont{j} findfont fs scalefont setfont s stringwidth pop /sw exch def\n\
               }} if\n\
               gsave {cx} {cy} translate {rotate} rotate\n\
               {c} {m} {ye} {k} setcmykcolor {ovp} setoverprint\n\
               {x} {y} moveto s show\n\
               grestore\n\
             }} def\n",
            shrink = spec.shrink,
            cx = f(p.centre.0 - media[0]),
            cy = f(p.centre.1 - media[1]),
            ovp = spec.overprint,
        ));
    }
    for row in rows {
        for (j, p) in placed.iter().enumerate() {
            let v = row.get(j).map(String::as_str).unwrap_or("");
            if is_barcode(p.field) {
                ps.push_str(&barcode_ps(p, v, media, rotate)?);
            } else {
                ps.push_str(&format!("{} MPshow{j} ", ps_hex(v)));
            }
        }
        ps.push_str("showpage\n");
    }
    Ok(ps)
}

fn is_barcode(field: &MergeField) -> bool {
    !field.kind.is_empty() && field.kind != "text"
}

/// PostScript drawing `value` as the field's barcode, as large as fits in
/// its box with the code's clear margin, centred. Each run of dark modules
/// is one filled rectangle, so the code stays sharp at any size.
fn barcode_ps(p: &Placed, value: &str, media: [f64; 4], rotate: i32) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let m = crate::barcode::encode(&p.field.kind, value)?;
    let f = |v: f64| format!("{v:.4}");
    let (bw, bh) = p.size;
    let q = m.quiet as f64;
    let module = (bw / (m.width as f64 + 2.0 * q)).min(bh / (m.height as f64 + 2.0 * q));
    let left = -(m.width as f64) * module / 2.0;
    let top = m.height as f64 * module / 2.0;
    let mut rects = String::new();
    for y in 0..m.height {
        let mut x = 0;
        while x < m.width {
            if !m.bits[y * m.width + x] {
                x += 1;
                continue;
            }
            let start = x;
            while x < m.width && m.bits[y * m.width + x] {
                x += 1;
            }
            rects.push_str(&format!(
                "{} {} {} {} ",
                f(left + start as f64 * module),
                f(top - (y + 1) as f64 * module),
                f((x - start) as f64 * module),
                f(module)
            ));
        }
        rects.push('\n');
    }
    let [c, mg, ye, k] = p.field.cmyk.map(|v| f((v / 100.0).clamp(0.0, 1.0)));
    Ok(format!(
        "gsave {cx} {cy} translate {rotate} rotate {c} {mg} {ye} {k} setcmykcolor {ovp} setoverprint\n[{rects}] rectfill grestore\n",
        cx = f(p.centre.0 - media[0]),
        cy = f(p.centre.1 - media[1]),
        ovp = p.field.overprint,
    ))
}

/// Replaces page `page` with one copy of it per row, each with the row's
/// values (one per field, in order) set in the fields' boxes. Returns the
/// number of pages made.
pub fn mail_merge(path: &Path, page: u32, rows: &[Vec<String>], spec: &MergeSpec) -> Result<u32, String> {
    if rows.is_empty() {
        return Err("The CSV file has no rows to merge.".into());
    }
    if spec.fields.is_empty() {
        return Err("Draw a box for the text first.".into());
    }
    // Check every barcode first, so a value that can't be encoded is
    // reported with its row.
    for (j, field) in spec.fields.iter().enumerate().filter(|(_, f)| is_barcode(f)) {
        for (i, row) in rows.iter().enumerate() {
            let v = row.get(j).map(|s| s.trim()).unwrap_or("");
            if !v.is_empty() {
                crate::barcode::encode(&field.kind, v).map_err(|e| format!("Row {}: {e}", i + 1))?;
            }
        }
    }
    let mut doc = load(path)?;
    let template = pagebox::page_id_for(&doc, page)?;
    own_inherited(&mut doc, template);
    let geom = page_geom(&doc, template);
    let media = geom.media;
    let (mw, mh) = (media[2] - media[0], media[3] - media[1]);
    let (dw, dh) = if geom.rotate == 90 || geom.rotate == 270 { (mh, mw) } else { (mw, mh) };
    let placed: Vec<Placed> = spec
        .fields
        .iter()
        .map(|field| {
            let [l, t, w, h] = field.rect;
            Placed { field, centre: geom.to_pdf(l + w / 2.0, t + h / 2.0), size: (w * dw, h * dh) }
        })
        .collect();

    // Typeset the values with Ghostscript: one page per row, fonts embedded.
    let tmp = tempfile::TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let ps_path = tmp.path().join("merge.ps");
    let pdf_path = tmp.path().join("merge.pdf");
    let font_dir = tmp.path().join("fonts");
    std::fs::create_dir_all(&font_dir).map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let fonts = spec.fields.iter().map(|f| font_name(&f.font, &font_dir)).collect::<Result<Vec<_>, _>>()?;
    let program = merge_program(rows, &placed, &fonts, media, geom.rotate)?;
    std::fs::write(&ps_path, program).map_err(|e| format!("Could not write the mail merge program: {e}"))?;
    gs::postscript_to_pdf(&ps_path, &pdf_path, &font_dir)?;
    let mut text = load(&pdf_path)?;
    let text_pages: Vec<ObjectId> = text.get_pages().into_values().collect();
    if text_pages.len() != rows.len() {
        return Err(format!("Expected {} pages of merged text but got {}.", rows.len(), text_pages.len()));
    }

    // Each typeset page becomes a form XObject in the document.
    let mut parts: Vec<(Vec<u8>, Object)> = Vec::new();
    for id in &text_pages {
        own_inherited(&mut text, *id);
    }
    text.renumber_objects_with(doc.max_id + 1);
    for id in text.get_pages().into_values() {
        let content = text.get_page_content(id);
        let res = text.get_dictionary(id).and_then(|d| d.get(b"Resources")).cloned().unwrap_or(Object::Dictionary(Dictionary::new()));
        parts.push((content, res));
    }
    let catalog = text.trailer.get(b"Root").and_then(|o| o.as_reference()).ok();
    for (id, obj) in std::mem::take(&mut text.objects) {
        if Some(id) != catalog {
            doc.objects.insert(id, obj);
        }
    }
    doc.max_id = doc.objects.keys().map(|k| k.0).max().unwrap_or(doc.max_id);
    let forms: Vec<ObjectId> = parts
        .into_iter()
        .map(|(content, res)| {
            let mut d = Dictionary::new();
            d.set("Type", Object::Name(b"XObject".to_vec()));
            d.set("Subtype", Object::Name(b"Form".to_vec()));
            d.set("BBox", Object::Array(vec![0.into(), 0.into(), Object::Real(mw as f32), Object::Real(mh as f32)]));
            d.set(
                "Matrix",
                Object::Array(vec![1.into(), 0.into(), 0.into(), 1.into(), Object::Real(media[0] as f32), Object::Real(media[1] as f32)]),
            );
            d.set("Resources", res);
            let mut s = Stream::new(d, content);
            let _ = s.compress();
            doc.add_object(s)
        })
        .collect();

    // The template's own resources (with XObjects inline, so each copy can
    // add its own text) and content.
    let tpl = doc.get_dictionary(template).map_err(err)?.clone();
    let mut res = match tpl.get(b"Resources") {
        Ok(o) => doc.dereference(o).and_then(|(_, o)| o.as_dict()).cloned().unwrap_or_default(),
        Err(_) => Dictionary::new(),
    };
    let xobjects = match res.get(b"XObject") {
        Ok(o) => doc.dereference(o).and_then(|(_, o)| o.as_dict()).cloned().unwrap_or_default(),
        Err(_) => Dictionary::new(),
    };
    res.remove(b"XObject");
    let contents: Vec<Object> = doc.get_page_contents(template).into_iter().map(Object::Reference).collect();
    let open = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));

    let mut copies = Vec::new();
    for form in forms {
        let mut page = tpl.clone();
        for key in [b"Annots".as_slice(), b"StructParents", b"B", b"Parent", b"Thumb"] {
            page.remove(key);
        }
        let mut xo = xobjects.clone();
        xo.set("MPmerge", Object::Reference(form));
        let mut r = res.clone();
        r.set("XObject", Object::Dictionary(xo));
        page.set("Resources", Object::Dictionary(r));
        let close = doc.add_object(Stream::new(Dictionary::new(), b"\nQ\nq /MPmerge Do Q\n".to_vec()));
        let mut c = vec![Object::Reference(open)];
        c.extend(contents.iter().cloned());
        c.push(Object::Reference(close));
        page.set("Contents", Object::Array(c));
        copies.push(doc.add_object(page));
    }
    insert_into_tree(&mut doc, page - 1, &copies)?;
    doc.delete_pages(&[page + copies.len() as u32]);
    save(doc, path)?;
    Ok(copies.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_first_column() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("a.csv");
        std::fs::write(&p, "\u{FEFF}Name,Town\r\n\"Smith, Jo\",Leeds\n\nO'Neil\n\"Say \"\"hi\"\"\"\n£5").unwrap();
        let rows = read_csv(&p, None).unwrap();
        let first: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(first, ["Name", "Smith, Jo", "O'Neil", "Say \"hi\"", "£5"]);
        assert_eq!(rows[1], ["Smith, Jo", "Leeds"]);
        std::fs::write(&p, "a;b;c\n1;2;3\n").unwrap();
        assert_eq!(read_csv(&p, None).unwrap()[1], ["1", "2", "3"]);
    }

    #[test]
    fn hex_latin1() {
        assert_eq!(ps_hex("A£€"), "<41A33F>");
    }

    /// `MP_PAGES_PDF=file.pdf cargo test pages_file -- --nocapture`
    #[test]
    fn pages_file() {
        let Ok(src) = std::env::var("MP_PAGES_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let n0 = pagebox::page_count(&p).unwrap();
        let n1 = insert_pdfs(&p, &[src.clone()], 1).unwrap();
        assert_eq!(n1, n0 * 2);
        let n2 = delete_page(&p, 1).unwrap();
        assert_eq!(n2, n1 - 1);
        assert!(delete_pages(&p, &(1..=n2).collect::<Vec<_>>()).is_err());
        let ids = |p: &Path| -> Vec<Vec<u8>> {
            let d = Document::load(p).unwrap();
            d.get_pages().values().map(|id| d.get_page_content(*id)).collect()
        };
        let before = ids(&p);
        move_page(&p, 1, 3).unwrap();
        let after = ids(&p);
        assert_eq!(after.len(), before.len());
        assert_eq!(after[2], before[0]);
        assert_eq!(after[0], before[1]);
        move_page(&p, 3, 1).unwrap();
        assert_eq!(ids(&p), before);
        let field = |rect: [f64; 4], align: &str| MergeField {
            kind: "text".into(),
            rect,
            font: "sans".into(),
            size: 24.0,
            cmyk: [0.0, 100.0, 0.0, 0.0],
            align: align.into(),
            valign: "middle".into(),
            shrink: true,
            overprint: false,
        };
        let mut second = field([0.1, 0.25, 0.5, 0.05], "left");
        // An installed font, where there is one to try.
        let georgia = Path::new(r"C:\Windows\Fonts\georgia.ttf");
        if georgia.exists() {
            second.font = format!("sys:{}", georgia.display());
        }
        let mut qr = field([0.6, 0.6, 0.3, 0.3], "centre");
        qr.kind = "qr".into();
        let mut dm = field([0.1, 0.6, 0.3, 0.3], "centre");
        dm.kind = "datamatrix".into();
        let spec = MergeSpec { fields: vec![field([0.1, 0.1, 0.5, 0.1], "centre"), second, qr, dm] };
        let rows: Vec<Vec<String>> = [["Alice", "Leeds", "https://mangoprint.co.uk", "A1"], ["Bob £5", "York", "Bob", "0123456789"], ["A very long name that will not fit the box at all", "", "", ""]]
            .iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect();
        let made = mail_merge(&p, 1, &rows, &spec).unwrap();
        assert_eq!(made, 3);
        assert_eq!(pagebox::page_count(&p).unwrap(), n2 + 2);
        let fonts = crate::fonts::list_fonts(&p).unwrap();
        for f in &fonts {
            println!("{} embedded={} pages={:?}", f.name, f.embedded, f.pages);
        }
        if let Ok(out) = std::env::var("MP_PAGES_OUT") {
            std::fs::copy(&p, out).unwrap();
        }
    }
}

#[cfg(test)]
mod sheet_tests {
    /// Prints the rows of the spreadsheet in MP_SHEET (if set).
    #[test]
    fn dump_sheet() {
        let Ok(p) = std::env::var("MP_SHEET") else { return };
        let rows = super::read_csv(std::path::Path::new(&p), None).unwrap();
        println!("{} rows", rows.len());
        for r in rows.iter().take(12) {
            println!("{r:?}");
        }
        for r in rows.iter().rev().take(3) {
            println!("{r:?}");
        }
        let widths: std::collections::BTreeMap<usize, usize> = rows.iter().fold(Default::default(), |mut m, r| {
            *m.entry(r.len()).or_default() += 1;
            m
        });
        println!("row lengths: {widths:?}");
    }
}
