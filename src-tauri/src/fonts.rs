//! Font check: every font the pages use (including inside form XObjects
//! and annotation-free page content), whether it's embedded in the PDF,
//! and which pages use it. A printer needs every font embedded; one that
//! isn't gets substituted on the RIP and the text may reflow or change.

use crate::colorcheck::deref;
use lopdf::{Dictionary, Document, Object, ObjectId};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

const MAX_FORM_DEPTH: u32 = 8;

#[derive(Serialize, Clone, Debug)]
pub struct FontInfo {
    /// Font name without the subset tag (e.g. "Poppins-Bold").
    pub name: String,
    /// "Type 1", "TrueType", "OpenType", "Type 3", "CID (Type 0)"…
    pub kind: String,
    pub embedded: bool,
    /// Only the characters used are embedded (normal for print PDFs).
    pub subset: bool,
    pub pages: Vec<u32>,
}

fn name_of(doc: &Document, d: &Dictionary, key: &[u8]) -> Option<String> {
    d.get(key).ok().map(|o| deref(doc, o)).and_then(|o| o.as_name().ok()).map(|n| String::from_utf8_lossy(n).into_owned())
}

/// (name, kind, embedded, subset) for a font dictionary.
fn describe(doc: &Document, font: &Dictionary) -> (String, String, bool, bool) {
    let subtype = name_of(doc, font, b"Subtype").unwrap_or_default();
    let raw = name_of(doc, font, b"BaseFont").or_else(|| name_of(doc, font, b"Name")).unwrap_or_else(|| "(unnamed)".into());
    // Subset fonts are named "ABCDEF+RealName".
    let (name, subset) = match raw.split_once('+') {
        Some((tag, rest)) if tag.len() == 6 && tag.chars().all(|c| c.is_ascii_uppercase()) => (rest.to_string(), true),
        _ => (raw, false),
    };

    // The descriptor that would hold the embedded font program.
    let descendant = if subtype == "Type0" {
        font.get(b"DescendantFonts")
            .ok()
            .map(|o| deref(doc, o))
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| deref(doc, o).as_dict().ok())
    } else {
        None
    };
    let desc_font = descendant.unwrap_or(font);
    let descriptor = desc_font.get(b"FontDescriptor").ok().and_then(|o| deref(doc, o).as_dict().ok());
    let file_key = descriptor.and_then(|d| {
        [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"].into_iter().find(|k| d.has(k))
    });
    let embedded = subtype == "Type3" || file_key.is_some();

    let file3_subtype = descriptor
        .and_then(|d| d.get(b"FontFile3").ok())
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_stream().ok())
        .and_then(|s| name_of(doc, &s.dict, b"Subtype"));
    let kind = match subtype.as_str() {
        "Type3" => "Type 3".to_string(),
        "Type0" => {
            let cid = descendant.and_then(|d| name_of(doc, d, b"Subtype")).unwrap_or_default();
            match (cid.as_str(), file3_subtype.as_deref()) {
                (_, Some("OpenType")) => "OpenType (CID)".into(),
                ("CIDFontType2", _) => "TrueType (CID)".into(),
                _ => "Type 1 (CID)".into(),
            }
        }
        "TrueType" => "TrueType".into(),
        "Type1" | "MMType1" => match file3_subtype.as_deref() {
            Some("OpenType") => "OpenType".into(),
            _ => "Type 1".into(),
        },
        "" => "Unknown".into(),
        other => other.to_string(),
    };
    (name, kind, embedded, subset)
}

struct Collector<'a> {
    doc: &'a Document,
    /// Font object (or a synthetic key for direct dictionaries) → info.
    fonts: BTreeMap<String, FontInfo>,
    seen_forms: HashSet<(ObjectId, u32)>,
}

impl<'a> Collector<'a> {
    fn add_resources(&mut self, res: &Dictionary, page: u32, depth: u32) {
        let doc = self.doc;
        if let Ok(Object::Dictionary(fonts)) = res.get_deref(b"Font", doc) {
            for (_, entry) in fonts.iter() {
                let Ok(font) = deref(doc, entry).as_dict() else { continue };
                let (name, kind, embedded, subset) = describe(doc, font);
                // The same font can appear as several objects (e.g. one per
                // subset); list it once per name/kind/embedding.
                let key = format!("{name}\0{kind}\0{embedded}");
                let info = self.fonts.entry(key).or_insert_with(|| FontInfo { name, kind, embedded, subset, pages: vec![] });
                info.subset &= subset;
                if info.pages.last() != Some(&page) && !info.pages.contains(&page) {
                    info.pages.push(page);
                }
            }
        }
        if depth >= MAX_FORM_DEPTH {
            return;
        }
        // Forms (grouped artwork, placed PDFs) and tiling patterns carry
        // their own resources.
        for category in [b"XObject".as_slice(), b"Pattern"] {
            let Ok(Object::Dictionary(items)) = res.get_deref(category, doc) else { continue };
            for (_, entry) in items.iter() {
                if let Object::Reference(id) = entry {
                    if !self.seen_forms.insert((*id, page)) {
                        continue;
                    }
                }
                let Ok(stream) = deref(doc, entry).as_stream() else { continue };
                if let Ok(Object::Dictionary(inner)) = stream.dict.get_deref(b"Resources", doc) {
                    self.add_resources(inner, page, depth + 1);
                }
            }
        }
    }
}

pub fn list_fonts(path: &Path) -> Result<Vec<FontInfo>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let mut c = Collector { doc: &doc, fonts: BTreeMap::new(), seen_forms: HashSet::new() };
    for (num, page_id) in doc.get_pages() {
        let Ok((own, inherited)) = doc.get_page_resources(page_id) else { continue };
        let mut all: Vec<&Dictionary> = own.into_iter().collect();
        all.extend(inherited.iter().filter_map(|id| doc.get_dictionary(*id).ok()));
        for res in all {
            c.add_resources(res, num, 0);
        }
    }
    let mut out: Vec<FontInfo> = c.fonts.into_values().collect();
    for f in &mut out {
        f.pages = f.pages.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
    }
    // Problems first, then by name.
    out.sort_by(|a, b| a.embedded.cmp(&b.embedded).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MP_TEST_PDF=path cargo test fonts_file -- --nocapture`
    #[test]
    fn fonts_file() {
        let Ok(p) = std::env::var("MP_TEST_PDF") else { return };
        for f in list_fonts(Path::new(&p)).unwrap() {
            println!("{f:?}");
        }
    }
}
