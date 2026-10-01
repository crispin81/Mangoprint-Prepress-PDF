//! Effective resolution of the raster images on a page, shown in the
//! toolbar as "Raster DPI of PDF". A PDF has no DPI of its own — only its
//! placed raster images do (vector art and text are resolution-independent
//! and aren't counted) — so we walk the page's content stream, track the transformation
//! matrix, and for every image XObject work out pixels ÷ placed size in
//! inches. Form XObjects are followed (with their /Matrix and /Resources).

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object};
use std::path::Path;

type Matrix = [f64; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
const MAX_FORM_DEPTH: u32 = 8;

/// `a` applied first, then `b` (PDF's `cm` is `new = a × current`).
fn mul(a: &Matrix, b: &Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(f) => Some(*f as f64),
        _ => None,
    }
}

fn matrix_from(objs: &[Object]) -> Option<Matrix> {
    if objs.len() != 6 {
        return None;
    }
    let v: Vec<f64> = objs.iter().filter_map(num).collect();
    (v.len() == 6).then(|| [v[0], v[1], v[2], v[3], v[4], v[5]])
}

/// Finds a named XObject in the given resource dictionaries (first match wins).
fn find_xobject<'a>(doc: &'a Document, resources: &[&'a Dictionary], name: &[u8]) -> Option<&'a lopdf::Stream> {
    for res in resources {
        let Ok(Object::Dictionary(xobjs)) = res.get_deref(b"XObject", doc) else { continue };
        let Ok(obj) = xobjs.get_deref(name, doc) else { continue };
        if let Object::Stream(s) = obj {
            return Some(s);
        }
    }
    None
}

/// Lowest and highest effective PPI seen so far.
type Range = Option<(f64, f64)>;

/// What a page contains: the raster image resolution range, and whether
/// there is any vector content (filled/stroked paths, shadings or text).
#[derive(Default)]
pub struct Scan {
    pub ppi: Range,
    pub vector: bool,
}

fn walk(doc: &Document, content: &[u8], resources: &[&Dictionary], start: Matrix, depth: u32, scan: &mut Scan) {
    let Ok(ops) = Content::decode(content) else { return };
    let mut ctm = start;
    let mut stack: Vec<Matrix> = Vec::new();

    for op in &ops.operations {
        match op.operator.as_str() {
            // Path painting (not `n`, which only clips), shadings and text.
            "f" | "F" | "f*" | "S" | "s" | "B" | "B*" | "b" | "b*" | "sh" | "Tj" | "TJ" | "'" | "\"" => {
                scan.vector = true;
            }
            "q" => stack.push(ctm),
            "Q" => ctm = stack.pop().unwrap_or(start),
            "cm" => {
                if let Some(m) = matrix_from(&op.operands) {
                    ctm = mul(&m, &ctm);
                }
            }
            "Do" => {
                let Some(Ok(name)) = op.operands.first().map(|o| o.as_name()) else { continue };
                let Some(stream) = find_xobject(doc, resources, name) else { continue };
                let subtype = stream.dict.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"");
                if subtype == b"Image" {
                    let w = stream.dict.get(b"Width").ok().and_then(num).unwrap_or(0.0);
                    let h = stream.dict.get(b"Height").ok().and_then(num).unwrap_or(0.0);
                    // The image fills the unit square, so its placed size in
                    // points is the length of the CTM's x and y basis vectors.
                    let placed_w = ctm[0].hypot(ctm[1]);
                    let placed_h = ctm[2].hypot(ctm[3]);
                    if w < 8.0 || h < 8.0 || placed_w < 2.0 || placed_h < 2.0 {
                        continue; // ignore tiny images/masks — they skew the result
                    }
                    let ppi = (w / (placed_w / 72.0)).min(h / (placed_h / 72.0));
                    scan.ppi = Some(scan.ppi.map_or((ppi, ppi), |(lo, hi)| (lo.min(ppi), hi.max(ppi))));
                } else if subtype == b"Form" && depth < MAX_FORM_DEPTH {
                    let form_m = stream
                        .dict
                        .get(b"Matrix")
                        .ok()
                        .and_then(|o| o.as_array().ok())
                        .and_then(|a| matrix_from(a))
                        .unwrap_or(IDENTITY);
                    let mut form_res: Vec<&Dictionary> = Vec::new();
                    if let Ok(Object::Dictionary(d)) = stream.dict.get_deref(b"Resources", doc) {
                        form_res.push(d);
                    }
                    // Forms without their own /Resources inherit the caller's.
                    form_res.extend_from_slice(resources);
                    let data = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
                    walk(doc, &data, &form_res, mul(&form_m, &ctm), depth + 1, scan);
                }
            }
            _ => {}
        }
    }
}

/// Scans `page` for placed raster images (lowest/highest effective pixels
/// per inch) and vector content.
pub fn scan_page(path: &Path, page: u32) -> Result<Scan, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = *doc
        .get_pages()
        .get(&page)
        .ok_or_else(|| format!("Page {page} does not exist in this PDF."))?;
    let content = doc.get_page_content(page_id);

    let (own, inherited) = doc.get_page_resources(page_id).map_err(|e| format!("Could not read page resources: {e}"))?;
    let mut resources: Vec<&Dictionary> = own.into_iter().collect();
    resources.extend(inherited.iter().filter_map(|id| doc.get_dictionary(*id).ok()));

    let mut scan = Scan::default();
    walk(&doc, &content, &resources, IDENTITY, 0, &mut scan);
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_order() {
        // scale by 2 then translate by (10, 0)
        let m = mul(&[2.0, 0.0, 0.0, 2.0, 0.0, 0.0], &[1.0, 0.0, 0.0, 1.0, 10.0, 0.0]);
        assert_eq!(m, [2.0, 0.0, 0.0, 2.0, 10.0, 0.0]);
    }

    /// Manual check against a real file: `MP_TEST_PDF=path cargo test -- --nocapture`.
    #[test]
    fn real_pdf_if_given() {
        let Ok(p) = std::env::var("MP_TEST_PDF") else { return };
        for page in 1..=3 {
            println!("page {page}: {:?}", scan_page(Path::new(&p), page).map(|s| (s.ppi, s.vector)));
        }
    }
}
