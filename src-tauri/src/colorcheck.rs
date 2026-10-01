//! RGB check: finds objects painted in an RGB colour space (DeviceRGB,
//! CalRGB, 3-component ICCBased, or an Indexed palette built on one of
//! those) by walking each page's content stream, including form XObjects.
//! Separation/DeviceN spot colours, CMYK and grey are not RGB.

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object};
use serde::Serialize;
use std::path::Path;

const MAX_FORM_DEPTH: u32 = 8;

#[derive(Serialize, Default, Clone, Debug)]
pub struct PageRgb {
    pub page: u32,
    pub text: u32,
    pub vector: u32,
    pub images: u32,
    pub shadings: u32,
}

impl PageRgb {
    fn total(&self) -> u32 {
        self.text + self.vector + self.images + self.shadings
    }
}

fn deref<'a>(doc: &'a Document, o: &'a Object) -> &'a Object {
    doc.dereference(o).map(|(_, o)| o).unwrap_or(o)
}

/// Is this colour space (a name or array, possibly a reference) RGB?
fn is_rgb_space(doc: &Document, cs: &Object, resources: &[&Dictionary], depth: u32) -> bool {
    if depth > 4 {
        return false;
    }
    match deref(doc, cs) {
        Object::Name(n) => match n.as_slice() {
            b"DeviceRGB" | b"RGB" => true,
            b"DeviceCMYK" | b"CMYK" | b"DeviceGray" | b"G" | b"Pattern" => false,
            // A named entry in the resources' /ColorSpace dictionary.
            other => resources.iter().any(|res| {
                let Ok(Object::Dictionary(spaces)) = res.get_deref(b"ColorSpace", doc) else { return false };
                spaces.get(other).map(|o| is_rgb_space(doc, o, resources, depth + 1)).unwrap_or(false)
            }),
        },
        Object::Array(arr) => {
            let Some(Ok(family)) = arr.first().map(|o| deref(doc, o).as_name()) else { return false };
            match family {
                b"CalRGB" => true,
                b"ICCBased" => arr
                    .get(1)
                    .and_then(|o| deref(doc, o).as_stream().ok())
                    .and_then(|s| s.dict.get(b"N").ok().and_then(|n| n.as_i64().ok()))
                    == Some(3),
                b"Indexed" | b"I" => arr.get(1).map(|b| is_rgb_space(doc, b, resources, depth + 1)).unwrap_or(false),
                // Patterns with an underlying space: [/Pattern /DeviceRGB].
                b"Pattern" => arr.get(1).map(|b| is_rgb_space(doc, b, resources, depth + 1)).unwrap_or(false),
                _ => false, // Separation, DeviceN, CalGray, Lab…
            }
        }
        _ => false,
    }
}

fn find_named<'a>(doc: &'a Document, resources: &[&'a Dictionary], category: &[u8], name: &[u8]) -> Option<&'a Object> {
    for res in resources {
        let Ok(Object::Dictionary(d)) = res.get_deref(category, doc) else { continue };
        if let Ok(o) = d.get_deref(name, doc) {
            return Some(o);
        }
    }
    None
}

#[derive(Clone, Copy)]
struct Gs {
    fill_rgb: bool,
    stroke_rgb: bool,
}

fn walk(doc: &Document, content: &[u8], resources: &[&Dictionary], depth: u32, out: &mut PageRgb) {
    let Ok(ops) = Content::decode(content) else { return };
    let mut gs = Gs { fill_rgb: false, stroke_rgb: false };
    let mut stack: Vec<Gs> = Vec::new();

    for op in &ops.operations {
        let first = op.operands.first();
        match op.operator.as_str() {
            "q" => stack.push(gs),
            "Q" => gs = stack.pop().unwrap_or(Gs { fill_rgb: false, stroke_rgb: false }),
            "rg" => gs.fill_rgb = true,
            "RG" => gs.stroke_rgb = true,
            "g" | "k" => gs.fill_rgb = false,
            "G" | "K" => gs.stroke_rgb = false,
            "cs" => gs.fill_rgb = first.map(|o| is_rgb_space(doc, o, resources, 0)).unwrap_or(false),
            "CS" => gs.stroke_rgb = first.map(|o| is_rgb_space(doc, o, resources, 0)).unwrap_or(false),
            "f" | "F" | "f*" => {
                if gs.fill_rgb {
                    out.vector += 1;
                }
            }
            "S" | "s" => {
                if gs.stroke_rgb {
                    out.vector += 1;
                }
            }
            "B" | "B*" | "b" | "b*" => {
                if gs.fill_rgb || gs.stroke_rgb {
                    out.vector += 1;
                }
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if gs.fill_rgb {
                    out.text += 1;
                }
            }
            // Inline image: lopdf hands back its dictionary as a stream.
            "BI" => {
                let Some(Object::Stream(img)) = first else { continue };
                let d = &img.dict;
                let is_mask = d.get(b"IM").or_else(|_| d.get(b"ImageMask")).and_then(|o| o.as_bool()).unwrap_or(false);
                let rgb = if is_mask {
                    gs.fill_rgb
                } else {
                    d.get(b"CS")
                        .or_else(|_| d.get(b"ColorSpace"))
                        .map(|cs| is_rgb_space(doc, cs, resources, 0))
                        .unwrap_or(false)
                };
                if rgb {
                    out.images += 1;
                }
            }
            "sh" => {
                let Some(Ok(name)) = first.map(|o| o.as_name()) else { continue };
                if let Some(sh) = find_named(doc, resources, b"Shading", name) {
                    let dict = match sh {
                        Object::Dictionary(d) => Some(d),
                        Object::Stream(s) => Some(&s.dict),
                        _ => None,
                    };
                    if let Some(cs) = dict.and_then(|d| d.get(b"ColorSpace").ok()) {
                        if is_rgb_space(doc, cs, resources, 0) {
                            out.shadings += 1;
                        }
                    }
                }
            }
            "Do" => {
                let Some(Ok(name)) = first.map(|o| o.as_name()) else { continue };
                let Some(Object::Stream(stream)) = find_named(doc, resources, b"XObject", name) else { continue };
                let subtype = stream.dict.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"");
                if subtype == b"Image" {
                    let is_mask = stream.dict.get(b"ImageMask").and_then(|o| o.as_bool()).unwrap_or(false);
                    let rgb = if is_mask {
                        gs.fill_rgb // stencil masks are painted in the fill colour
                    } else {
                        stream.dict.get(b"ColorSpace").map(|cs| is_rgb_space(doc, cs, resources, 0)).unwrap_or(false)
                    };
                    if rgb {
                        out.images += 1;
                    }
                } else if subtype == b"Form" && depth < MAX_FORM_DEPTH {
                    let mut form_res: Vec<&Dictionary> = Vec::new();
                    if let Ok(Object::Dictionary(d)) = stream.dict.get_deref(b"Resources", doc) {
                        form_res.push(d);
                    }
                    form_res.extend_from_slice(resources);
                    let data = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
                    walk(doc, &data, &form_res, depth + 1, out);
                }
            }
            _ => {}
        }
    }
}

/// Pages that use RGB, with a count of each kind of RGB object.
pub fn check_rgb(path: &Path) -> Result<Vec<PageRgb>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let mut pages = Vec::new();
    for (num, page_id) in doc.get_pages() {
        let content = doc.get_page_content(page_id);
        let (own, inherited) = doc.get_page_resources(page_id).map_err(|e| format!("Could not read page resources: {e}"))?;
        let mut resources: Vec<&Dictionary> = own.into_iter().collect();
        resources.extend(inherited.iter().filter_map(|id| doc.get_dictionary(*id).ok()));
        let mut found = PageRgb { page: num, ..Default::default() };
        walk(&doc, &content, &resources, 0, &mut found);
        if found.total() > 0 {
            pages.push(found);
        }
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check: `MP_TEST_PDF=path cargo test check_rgb_file -- --nocapture`.
    #[test]
    fn check_rgb_file() {
        let Ok(p) = std::env::var("MP_TEST_PDF") else { return };
        println!("{:?}", check_rgb(Path::new(&p)));
    }
}
