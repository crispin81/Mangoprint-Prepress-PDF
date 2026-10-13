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

/// Deletes one page. Returns the new page count.
pub fn delete_page(path: &Path, page: u32) -> Result<u32, String> {
    let mut doc = load(path)?;
    let count = doc.get_pages().len() as u32;
    if count <= 1 {
        return Err("A PDF needs at least one page, so the last page can't be deleted.".into());
    }
    pagebox::page_id_for(&doc, page)?;
    doc.delete_pages(&[page]);
    save(doc, path)?;
    Ok(count - 1)
}

// ---------------------------------------------------------------------------
// Mail merge

/// The values in a CSV file's first column, one per row, blank rows left out.
pub fn read_csv(path: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read the CSV file: {e}"))?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    // UTF-8, or (as Excel often saves) a Windows / Latin-1 file.
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    };
    let mut out = Vec::new();
    let mut field = String::new();
    let mut first = true; // still in the row's first field
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let end_row = |field: &mut String, first: &mut bool, out: &mut Vec<String>| {
        let v = field.trim().to_string();
        if !v.is_empty() {
            out.push(v);
        }
        field.clear();
        *first = true;
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    if first {
                        field.push('"');
                    }
                } else {
                    quoted = false;
                }
            } else if first {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' | ';' | '\t' => first = false,
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                end_row(&mut field, &mut first, &mut out);
            }
            '\n' => end_row(&mut field, &mut first, &mut out),
            _ if first => field.push(c),
            _ => {}
        }
    }
    end_row(&mut field, &mut first, &mut out);
    Ok(out)
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MergeSpec {
    /// The text box as [left, top, width, height] of the displayed page.
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

/// A PostScript program drawing one page per value, each the size of the
/// template page's MediaBox, with the value set in the box.
fn merge_program(values: &[String], spec: &MergeSpec, media: [f64; 4], centre: (f64, f64), box_wh: (f64, f64), rotate: i32) -> String {
    let font = match spec.font.as_str() {
        "sans-bold" => "NimbusSans-Bold",
        "serif" => "NimbusRoman-Regular",
        "mono" => "NimbusMonoPS-Regular",
        _ => "NimbusSans-Regular",
    };
    let f = |v: f64| format!("{v:.4}");
    let (bw, bh) = box_wh;
    let x = match spec.align.as_str() {
        "left" => "BW -2 div".to_string(),
        "right" => "BW 2 div sw sub".to_string(),
        _ => "sw -2 div".to_string(),
    };
    let y = match spec.valign.as_str() {
        "top" => "BH 2 div fs 0.72 mul sub",
        "bottom" => "BH -2 div fs 0.22 mul add",
        _ => "fs 0.35 mul neg",
    };
    let [c, m, ye, k] = spec.cmyk.map(|v| (v / 100.0).clamp(0.0, 1.0));
    let mut ps = format!(
        "%!PS\n\
         << /PageSize [{pw} {ph}] >> setpagedevice\n\
         /MPFont /{font} findfont dup length dict begin\n\
         {{ 1 index /FID ne {{ def }} {{ pop pop }} ifelse }} forall\n\
         /Encoding ISOLatin1Encoding def currentdict end definefont pop\n\
         /BW {bw} def /BH {bh} def /SIZE {size} def\n\
         /MPshow {{\n\
           /s exch def /fs SIZE def\n\
           /MPFont findfont fs scalefont setfont s stringwidth pop /sw exch def\n\
           {shrink} sw BW gt and {{\n\
             /fs SIZE BW mul sw div def\n\
             /MPFont findfont fs scalefont setfont s stringwidth pop /sw exch def\n\
           }} if\n\
           gsave {cx} {cy} translate {rotate} rotate\n\
           {c} {m} {ye} {k} setcmykcolor {ovp} setoverprint\n\
           {x} {y} moveto s show\n\
           grestore showpage\n\
         }} def\n",
        pw = f(media[2] - media[0]),
        ph = f(media[3] - media[1]),
        bw = f(bw),
        bh = f(bh),
        size = f(spec.size.max(0.5)),
        shrink = spec.shrink,
        cx = f(centre.0 - media[0]),
        cy = f(centre.1 - media[1]),
        rotate = rotate,
        c = f(c),
        m = f(m),
        ye = f(ye),
        k = f(k),
        ovp = spec.overprint,
    );
    for v in values {
        ps.push_str(&ps_hex(v));
        ps.push_str(" MPshow\n");
    }
    ps
}

/// Replaces page `page` with one copy of it per value, each with the value
/// set in the box described by `spec`. Returns the number of pages made.
pub fn mail_merge(path: &Path, page: u32, values: &[String], spec: &MergeSpec) -> Result<u32, String> {
    if values.is_empty() {
        return Err("The CSV file has no values to merge.".into());
    }
    let mut doc = load(path)?;
    let template = pagebox::page_id_for(&doc, page)?;
    own_inherited(&mut doc, template);
    let geom = page_geom(&doc, template);
    let media = geom.media;
    let (mw, mh) = (media[2] - media[0], media[3] - media[1]);
    let (dw, dh) = if geom.rotate == 90 || geom.rotate == 270 { (mh, mw) } else { (mw, mh) };
    let [l, t, w, h] = spec.rect;
    let centre = geom.to_pdf(l + w / 2.0, t + h / 2.0);

    // Typeset the values with Ghostscript: one page each, fonts embedded.
    let tmp = tempfile::TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let ps_path = tmp.path().join("merge.ps");
    let pdf_path = tmp.path().join("merge.pdf");
    let program = merge_program(values, spec, media, centre, (w * dw, h * dh), geom.rotate);
    std::fs::write(&ps_path, program).map_err(|e| format!("Could not write the mail merge program: {e}"))?;
    gs::postscript_to_pdf(&ps_path, &pdf_path)?;
    let mut text = load(&pdf_path)?;
    let text_pages: Vec<ObjectId> = text.get_pages().into_values().collect();
    if text_pages.len() != values.len() {
        return Err(format!("Expected {} pages of merged text but got {}.", values.len(), text_pages.len()));
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
        std::fs::write(&p, "\u{FEFF}Name\r\n\"Smith, Jo\",x\n\nO'Neil\n\"Say \"\"hi\"\"\"\n£5").unwrap();
        assert_eq!(read_csv(&p).unwrap(), ["Name", "Smith, Jo", "O'Neil", "Say \"hi\"", "£5"]);
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
        let spec = MergeSpec {
            rect: [0.1, 0.1, 0.5, 0.1],
            font: "sans".into(),
            size: 24.0,
            cmyk: [0.0, 100.0, 0.0, 0.0],
            align: "centre".into(),
            valign: "middle".into(),
            shrink: true,
            overprint: false,
        };
        let vals: Vec<String> = ["Alice", "Bob £5", "A very long name that will not fit the box at all"].iter().map(|s| s.to_string()).collect();
        let made = mail_merge(&p, 1, &vals, &spec).unwrap();
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
