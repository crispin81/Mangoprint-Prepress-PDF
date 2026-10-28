//! Colour profiles: the ICC profiles embedded in a PDF. The output intent
//! (the press condition a PDF/X file was made for, e.g. "Coated FOGRA39")
//! and every ICCBased colour space the pages use (images, artwork,
//! transparency groups), with what each profile header says about itself.
//! Only reads what's in the file; no profiles are shipped with the app.

use crate::colorcheck::deref;
use lopdf::{Dictionary, Document, Object, ObjectId};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Serialize, Clone, Debug)]
pub struct IccProfile {
    /// The profile stream's object, "12 0", for saving it out.
    pub id: String,
    /// The profile's own description, e.g. "Coated FOGRA39 (ISO 12647-2:2004)".
    pub name: String,
    /// "CMYK", "RGB", "Grey", "Lab"…
    pub colour_space: String,
    /// "Output (printer)", "Display", "Input (scanner/camera)", "Colour space"…
    pub class: String,
    /// ICC version, e.g. "2.1" or "4.3".
    pub version: String,
    /// Creation date from the header, "2004-08-17".
    pub created: String,
    pub copyright: String,
    pub size: usize,
    /// It's the PDF's output intent (the press condition).
    pub output_intent: bool,
    /// Output intent details: condition identifier and its description.
    pub condition: String,
    pub condition_info: String,
    /// "PDF/X", "PDF/A"… from the output intent's subtype.
    pub standard: String,
    /// Pages whose content uses it as a colour space.
    pub pages: Vec<u32>,
}

/// What an ICC profile's header and tags say about it.
struct Header {
    name: String,
    colour_space: String,
    class: String,
    version: String,
    created: String,
    copyright: String,
}

fn sig(b: &[u8], at: usize) -> String {
    b.get(at..at + 4).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
}

fn u32_at(b: &[u8], at: usize) -> Option<usize> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes([s[0], s[1]]))
}

/// The text of a 'desc' or 'cprt' tag: v2 textDescriptionType ('desc'),
/// textType ('text') or v4 multiLocalizedUnicode ('mluc', first record).
fn tag_text(b: &[u8], offset: usize, size: usize) -> Option<String> {
    let t = b.get(offset..offset.checked_add(size)?)?;
    let clean = |s: String| s.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
    match t.get(0..4)? {
        b"desc" => {
            let n = u32_at(t, 8)?;
            Some(clean(String::from_utf8_lossy(t.get(12..12 + n)?).into_owned()))
        }
        b"text" => Some(clean(String::from_utf8_lossy(t.get(8..)?).into_owned())),
        b"mluc" => {
            let count = u32_at(t, 8)?;
            if count == 0 {
                return None;
            }
            let len = u32_at(t, 20)?;
            let off = u32_at(t, 24)?;
            let units: Vec<u16> = t.get(off..off + len)?.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            Some(clean(String::from_utf16_lossy(&units)))
        }
        _ => None,
    }
}

fn read_header(b: &[u8]) -> Header {
    let class = match sig(b, 12).as_str() {
        "prtr" => "Output (printer)",
        "mntr" => "Display",
        "scnr" => "Input (scanner/camera)",
        "spac" => "Colour space",
        "link" => "Device link",
        "abst" => "Abstract",
        "nmcl" => "Named colour",
        _ => "Unknown",
    }
    .to_string();
    let colour_space = match sig(b, 16).as_str() {
        "CMYK" => "CMYK".to_string(),
        "RGB" => "RGB".to_string(),
        "GRAY" => "Grey".to_string(),
        "Lab" => "Lab".to_string(),
        "XYZ" => "XYZ".to_string(),
        "" => "Unknown".to_string(),
        other => other.to_string(),
    };
    let version = b.get(8..10).map(|v| format!("{}.{}", v[0], v[1] >> 4)).unwrap_or_default();
    let created = match (u16_at(b, 24), u16_at(b, 26), u16_at(b, 28)) {
        (Some(y), Some(m), Some(d)) if y > 1900 && (1..=12).contains(&m) => format!("{y:04}-{m:02}-{d:02}"),
        _ => String::new(),
    };
    let (mut name, mut copyright) = (String::new(), String::new());
    if let Some(count) = u32_at(b, 128) {
        for i in 0..count.min(200) {
            let at = 132 + i * 12;
            let (Some(off), Some(size)) = (u32_at(b, at + 4), u32_at(b, at + 8)) else { break };
            match b.get(at..at + 4) {
                Some(b"desc") => name = tag_text(b, off, size).unwrap_or_default(),
                Some(b"cprt") => copyright = tag_text(b, off, size).unwrap_or_default(),
                _ => {}
            }
        }
    }
    Header { name, colour_space, class, version, created, copyright }
}

fn text_of(doc: &Document, d: &Dictionary, key: &[u8]) -> String {
    match d.get(key).map(|o| deref(doc, o)) {
        Ok(Object::String(s, _)) => {
            // UTF-16 with a BOM, or PDFDocEncoding (close enough to Latin-1).
            if s.starts_with(&[0xFE, 0xFF]) {
                let units: Vec<u16> = s[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                String::from_utf16_lossy(&units)
            } else {
                s.iter().map(|&c| c as char).collect()
            }
        }
        Ok(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        _ => String::new(),
    }
    .trim()
    .to_string()
}

/// Finds ICC profile streams ([/ICCBased <stream>] colour spaces) inside
/// an object, following references. Results per object are remembered, so
/// shared resources are only searched once.
struct Finder<'a> {
    doc: &'a Document,
    memo: HashMap<ObjectId, BTreeSet<ObjectId>>,
}

/// Keys that lead away from a page's resources (back up the page tree or
/// into the structure tree), not into colour spaces.
const SKIP_KEYS: [&[u8]; 6] = [b"Parent", b"P", b"StructParent", b"StructParents", b"Annots", b"Metadata"];

impl Finder<'_> {
    fn find(&mut self, obj: &Object, out: &mut BTreeSet<ObjectId>, depth: u32) {
        if depth > 40 {
            return;
        }
        match obj {
            Object::Reference(id) => {
                if let Some(found) = self.memo.get(id) {
                    out.extend(found.iter().copied());
                    return;
                }
                self.memo.insert(*id, BTreeSet::new()); // in progress: breaks cycles
                let mut found = BTreeSet::new();
                if let Ok(o) = self.doc.get_object(*id) {
                    let o = o.clone();
                    self.find(&o, &mut found, depth + 1);
                }
                out.extend(found.iter().copied());
                self.memo.insert(*id, found);
            }
            Object::Array(items) => {
                if let (Some(Object::Name(n)), Some(Object::Reference(id))) = (items.first(), items.get(1)) {
                    if n == b"ICCBased" {
                        out.insert(*id);
                        return;
                    }
                }
                for i in items {
                    self.find(i, out, depth + 1);
                }
            }
            Object::Dictionary(d) => self.find_dict(d, out, depth),
            // A stream's dictionary only (images, forms, shadings); its data is never searched.
            Object::Stream(s) => self.find_dict(&s.dict, out, depth),
            _ => {}
        }
    }

    fn find_dict(&mut self, d: &Dictionary, out: &mut BTreeSet<ObjectId>, depth: u32) {
        // A profile's own stream may name an /Alternate space; not a use.
        for (k, v) in d.iter() {
            if SKIP_KEYS.contains(&k.as_slice()) {
                continue;
            }
            self.find(v, out, depth + 1);
        }
    }
}

/// An ICC profile's details from its bytes (no PDF context), or None when
/// the bytes aren't a profile.
pub fn describe_bytes(data: &[u8]) -> Option<IccProfile> {
    if data.len() < 128 || data.get(36..40) != Some(b"acsp") {
        return None;
    }
    let h = read_header(data);
    Some(IccProfile {
        id: String::new(),
        name: if h.name.is_empty() { "(no description)".into() } else { h.name },
        colour_space: h.colour_space,
        class: h.class,
        version: h.version,
        created: h.created,
        copyright: h.copyright,
        size: data.len(),
        output_intent: false,
        condition: String::new(),
        condition_info: String::new(),
        standard: String::new(),
        pages: Vec::new(),
    })
}

pub fn list_profiles(path: &Path) -> Result<Vec<IccProfile>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let mut profiles: BTreeMap<ObjectId, IccProfile> = BTreeMap::new();
    let make = |doc: &Document, id: ObjectId| -> Option<IccProfile> {
        let stream = doc.get_object(id).ok()?.as_stream().ok()?;
        let data = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
        let mut p = describe_bytes(&data)?;
        p.id = format!("{} {}", id.0, id.1);
        Some(p)
    };
    // The output intents, in the catalog.
    if let Ok(catalog) = doc.catalog() {
        if let Ok(Object::Array(intents)) = catalog.get(b"OutputIntents").map(|o| deref(&doc, o)) {
            for intent in intents {
                let Ok(d) = deref(&doc, intent).as_dict() else { continue };
                let Ok(Object::Reference(pid)) = d.get(b"DestOutputProfile") else { continue };
                let Some(mut p) = profiles.remove(pid).or_else(|| make(&doc, *pid)) else { continue };
                p.output_intent = true;
                p.condition = text_of(&doc, d, b"OutputConditionIdentifier");
                p.condition_info = {
                    let info = text_of(&doc, d, b"Info");
                    if info.is_empty() { text_of(&doc, d, b"OutputCondition") } else { info }
                };
                p.standard = match text_of(&doc, d, b"S").as_str() {
                    "GTS_PDFX" => "PDF/X".into(),
                    "GTS_PDFA1" => "PDF/A".into(),
                    "ISO_PDFE1" => "PDF/E".into(),
                    other => other.to_string(),
                };
                profiles.insert(*pid, p);
            }
        }
    }

    // Colour spaces the pages use.
    let mut finder = Finder { doc: &doc, memo: HashMap::new() };
    for (num, page_id) in doc.get_pages() {
        let Ok((own, inherited)) = doc.get_page_resources(page_id) else { continue };
        let mut found = BTreeSet::new();
        if let Some(res) = own {
            finder.find_dict(res, &mut found, 0);
        }
        for id in inherited {
            finder.find(&Object::Reference(id), &mut found, 0);
        }
        // A page-level transparency group can name a blending colour space.
        if let Ok(page) = doc.get_dictionary(page_id) {
            if let Ok(g) = page.get(b"Group") {
                finder.find(g, &mut found, 0);
            }
        }
        for pid in found {
            if !profiles.contains_key(&pid) {
                let Some(p) = make(&doc, pid) else { continue };
                profiles.insert(pid, p);
            }
            if let Some(p) = profiles.get_mut(&pid) {
                if !p.pages.contains(&num) {
                    p.pages.push(num);
                }
            }
        }
    }

    // The same profile is often embedded many times (once per image); list
    // it once, with all its pages.
    let mut merged: Vec<IccProfile> = Vec::new();
    for p in profiles.into_values() {
        if let Some(m) = merged.iter_mut().find(|m| m.name == p.name && m.size == p.size && m.colour_space == p.colour_space) {
            m.pages.extend(p.pages);
            if p.output_intent {
                m.output_intent = true;
                m.condition = p.condition;
                m.condition_info = p.condition_info;
                m.standard = p.standard;
                m.id = p.id;
            }
        } else {
            merged.push(p);
        }
    }
    for p in &mut merged {
        p.pages.sort_unstable();
        p.pages.dedup();
    }
    // The output intent first, then CMYK, RGB, the rest; by name.
    let rank = |p: &IccProfile| (!p.output_intent, match p.colour_space.as_str() { "CMYK" => 0, "RGB" => 1, "Grey" => 2, _ => 3 });
    merged.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(merged)
}

/// Where a new output intent's profile comes from.
pub enum IntentSource {
    /// A profile already embedded in the PDF ("12 0").
    Embedded(String),
    /// An .icc file the user chose.
    File(std::path::PathBuf),
    /// No output intent: remove it.
    None,
}

/// The current output intent's profile stream, if the PDF has one.
fn current_intent(doc: &Document) -> Option<ObjectId> {
    doc.catalog()
        .ok()
        .and_then(|c| c.get(b"OutputIntents").ok())
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
        .and_then(|a| a.first())
        .and_then(|o| deref(doc, o).as_dict().ok())
        .and_then(|d| d.get(b"DestOutputProfile").ok())
        .and_then(|o| o.as_reference().ok())
}

/// Changes the output intent to `source`. When the PDF already has an
/// output intent and `convert` is set, the colours are first converted
/// from the old press to the new one (CMYK recalculated, 100% black kept),
/// as Acrobat's Convert Colors does; without an old output intent there's
/// nothing to convert from, so only the label is set. Returns whether the
/// colours were converted.
pub fn change_output_intent(path: &Path, source: IntentSource, convert: bool) -> Result<bool, String> {
    if !convert || matches!(source, IntentSource::None) {
        set_output_intent(path, source)?;
        return Ok(false);
    }
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let Some(old) = current_intent(&doc) else {
        drop(doc);
        set_output_intent(path, source)?;
        return Ok(false);
    };
    let tmp = tempfile::TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
    let from = tmp.path().join("from.icc");
    std::fs::write(&from, profile_bytes(path, &format!("{} {}", old.0, old.1))?).map_err(|e| format!("Could not write the profile: {e}"))?;
    let to = match &source {
        IntentSource::Embedded(id) => {
            if *id == format!("{} {}", old.0, old.1) {
                return Ok(false); // already the output intent
            }
            let p = tmp.path().join("to.icc");
            std::fs::write(&p, profile_bytes(path, id)?).map_err(|e| format!("Could not write the profile: {e}"))?;
            p
        }
        IntentSource::File(f) => f.clone(),
        IntentSource::None => unreachable!(),
    };
    let to_info = std::fs::read(&to).ok().and_then(|b| describe_bytes(&b)).ok_or("That isn't an ICC colour profile.")?;
    if to_info.colour_space != "CMYK" {
        return Err(format!("{} is a {} profile; an output intent for print needs a CMYK profile.", to_info.name, to_info.colour_space));
    }
    drop(doc);
    let converted = tmp.path().join("converted.pdf");
    crate::gs::convert_cmyk_profile(path, &converted, &from, &to)?;
    std::fs::copy(&converted, path).map_err(|e| format!("Could not update the working copy: {e}"))?;
    // The rewrite dropped the old profiles; embed the new one as the intent.
    set_output_intent(path, IntentSource::File(to))?;
    Ok(true)
}

/// Sets (or removes) the PDF's output intent. The CMYK values in the
/// file are not touched; this only records which press and paper they
/// were made for. Saved in place.
pub fn set_output_intent(path: &Path, source: IntentSource) -> Result<(), String> {
    let mut doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let root = doc.trailer.get(b"Root").and_then(|o| o.as_reference()).map_err(|_| "The PDF has no catalog.".to_string())?;
    let profile: Option<(ObjectId, String)> = match source {
        IntentSource::None => None,
        IntentSource::Embedded(id) => {
            let mut parts = id.split_whitespace().map(|s| s.parse::<u32>());
            let (Some(Ok(n)), Some(Ok(g))) = (parts.next(), parts.next()) else { return Err("Unknown profile.".into()) };
            let pid = (n, g as u16);
            // Already the output intent: keep it as it is (with its
            // registered condition name).
            let current = doc
                .catalog()
                .ok()
                .and_then(|c| c.get(b"OutputIntents").ok())
                .map(|o| deref(&doc, o))
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .and_then(|o| deref(&doc, o).as_dict().ok())
                .and_then(|d| d.get(b"DestOutputProfile").ok())
                .and_then(|o| o.as_reference().ok());
            if current == Some(pid) {
                return Ok(());
            }
            let bytes = profile_bytes(path, &id)?;
            let p = describe_bytes(&bytes).ok_or("That isn't an ICC colour profile.")?;
            if p.colour_space != "CMYK" {
                return Err(format!("{} is a {} profile; an output intent for print needs a CMYK profile.", p.name, p.colour_space));
            }
            Some((pid, p.name))
        }
        IntentSource::File(f) => {
            let bytes = std::fs::read(&f).map_err(|e| format!("Could not read the profile: {e}"))?;
            let p = describe_bytes(&bytes).ok_or("That file isn't an ICC colour profile.")?;
            if p.colour_space != "CMYK" {
                return Err(format!("{} is a {} profile; an output intent for print needs a CMYK profile.", p.name, p.colour_space));
            }
            let mut d = Dictionary::new();
            d.set("N", Object::Integer(4));
            d.set("Alternate", Object::Name(b"DeviceCMYK".to_vec()));
            let mut s = lopdf::Stream::new(d, bytes);
            let _ = s.compress();
            Some((doc.add_object(s), p.name))
        }
    };
    let catalog = doc.get_dictionary_mut(root).map_err(|_| "The PDF has no catalog.".to_string())?;
    match profile {
        None => {
            catalog.remove(b"OutputIntents");
        }
        Some((pid, name)) => {
            let mut intent = Dictionary::new();
            intent.set("Type", Object::Name(b"OutputIntent".to_vec()));
            intent.set("S", Object::Name(b"GTS_PDFX".to_vec()));
            // Not one of the registered conditions, so "Custom" plus the
            // profile's own name, as PDF/X allows.
            intent.set("OutputConditionIdentifier", Object::string_literal("Custom"));
            intent.set("OutputCondition", Object::string_literal(name.as_str()));
            intent.set("Info", Object::string_literal(name.as_str()));
            intent.set("DestOutputProfile", Object::Reference(pid));
            catalog.set("OutputIntents", Object::Array(vec![Object::Dictionary(intent)]));
        }
    }
    doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;
    Ok(())
}

/// The raw bytes of profile `id` ("12 0"), for saving as an .icc file.
pub fn profile_bytes(path: &Path, id: &str) -> Result<Vec<u8>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let mut parts = id.split_whitespace().map(|s| s.parse::<u32>());
    let (Some(Ok(n)), Some(Ok(g))) = (parts.next(), parts.next()) else { return Err("Unknown profile.".into()) };
    let stream = doc
        .get_object((n, g as u16))
        .ok()
        .and_then(|o| o.as_stream().ok())
        .ok_or("That profile isn't in the PDF any more.")?;
    Ok(stream.decompressed_content().unwrap_or_else(|_| stream.content.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MP_ICC_PDF=file.pdf cargo test icc_file -- --nocapture`
    #[test]
    fn icc_file() {
        let Ok(p) = std::env::var("MP_ICC_PDF") else { return };
        for prof in list_profiles(Path::new(&p)).unwrap() {
            println!("{prof:?}");
        }
    }

    /// An output intent pointing at an embedded profile is reported as one.
    #[test]
    fn output_intent_file() {
        let Ok(p) = std::env::var("MP_ICC_PDF") else { return };
        let mut doc = Document::load(&p).unwrap();
        let first = list_profiles(Path::new(&p)).unwrap().into_iter().find(|x| x.colour_space == "CMYK").unwrap();
        let mut parts = first.id.split(' ').map(|n| n.parse::<u32>().unwrap());
        let pid: ObjectId = (parts.next().unwrap(), parts.next().unwrap() as u16);
        let mut intent = Dictionary::new();
        intent.set("Type", Object::Name(b"OutputIntent".to_vec()));
        intent.set("S", Object::Name(b"GTS_PDFX".to_vec()));
        intent.set("OutputConditionIdentifier", Object::string_literal("FOGRA39"));
        intent.set("Info", Object::string_literal("Coated FOGRA39"));
        intent.set("DestOutputProfile", Object::Reference(pid));
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(root).unwrap().set("OutputIntents", Object::Array(vec![Object::Dictionary(intent)]));
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("x.pdf");
        doc.save(&out).unwrap();
        let list = list_profiles(&out).unwrap();
        println!("{:?}", list[0]);
        assert!(list[0].output_intent && list[0].standard == "PDF/X" && list[0].condition == "FOGRA39");
        assert_eq!(profile_bytes(&out, &list[0].id).unwrap().len(), list[0].size);
    }

    /// Setting an output intent from an embedded profile, then from an
    /// .icc file (MP_ICC_FILE), then removing it.
    #[test]
    fn set_intent_file() {
        let (Ok(p), Ok(icc)) = (std::env::var("MP_ICC_PDF"), std::env::var("MP_ICC_FILE")) else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("x.pdf");
        std::fs::copy(&p, &out).unwrap();
        let cmyk = list_profiles(&out).unwrap().into_iter().find(|x| x.colour_space == "CMYK").unwrap();
        set_output_intent(&out, IntentSource::Embedded(cmyk.id.clone())).unwrap();
        let l = list_profiles(&out).unwrap();
        assert!(l[0].output_intent && l[0].name == cmyk.name && l[0].condition == "Custom");
        set_output_intent(&out, IntentSource::File(icc.clone().into())).unwrap();
        let l = list_profiles(&out).unwrap();
        println!("{:?}", l[0]);
        assert!(l[0].output_intent && l[0].colour_space == "CMYK" && l[0].name != cmyk.name);
        set_output_intent(&out, IntentSource::None).unwrap();
        assert!(!list_profiles(&out).unwrap().iter().any(|x| x.output_intent));
    }

    /// Converting from one press (MP_ICC_FILE) to another (MP_ICC_FILE2)
    /// changes the CMYK of MP_CM_PDF but keeps 100% K black.
    #[test]
    fn convert_intent_file() {
        let (Ok(p), Ok(a), Ok(b)) = (std::env::var("MP_CM_PDF"), std::env::var("MP_ICC_FILE"), std::env::var("MP_ICC_FILE2")) else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("x.pdf");
        std::fs::copy(&p, &out).unwrap();
        assert!(!change_output_intent(&out, IntentSource::File(a.into()), true).unwrap(), "no old intent: label only");
        assert!(change_output_intent(&out, IntentSource::File(b.clone().into()), true).unwrap());
        let doc = Document::load(&out).unwrap();
        let content = doc.get_page_content(*doc.get_pages().get(&1).unwrap());
        let text = String::from_utf8_lossy(&content);
        println!("{text}");
        assert!(!text.contains("0 1 0 0 k"), "magenta was converted");
        assert!(text.contains("0 g") || text.contains("0 0 0 1 k"), "black stays pure black");
        let l = list_profiles(&out).unwrap();
        println!("{:?}", l[0]);
        assert!(l[0].output_intent);
    }

    #[test]
    fn v2_and_v4_text() {
        // 'desc' (v2): type, reserved, length 5, "Hello\0".
        let mut desc = b"desc\0\0\0\0".to_vec();
        desc.extend_from_slice(&6u32.to_be_bytes());
        desc.extend_from_slice(b"Hello\0");
        assert_eq!(tag_text(&desc, 0, desc.len()).as_deref(), Some("Hello"));
        // 'mluc' (v4): one record, en-US, 4 bytes "Hi" at offset 28.
        let mut m = b"mluc\0\0\0\0".to_vec();
        m.extend_from_slice(&1u32.to_be_bytes());
        m.extend_from_slice(&12u32.to_be_bytes());
        m.extend_from_slice(b"enUS");
        m.extend_from_slice(&4u32.to_be_bytes());
        m.extend_from_slice(&28u32.to_be_bytes());
        m.extend_from_slice(&[0, b'H', 0, b'i']);
        assert_eq!(tag_text(&m, 0, m.len()).as_deref(), Some("Hi"));
    }
}
