//! White overprint check: finds white objects (no ink in CMYK, grey or
//! spot tints, RGB white, or a spot named "White") painted with overprint
//! switched on. On press, overprinted white simply disappears.

use crate::colorcheck::{deref, find_named};
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

const MAX_FORM_DEPTH: u32 = 8;

#[derive(Serialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OverprintHit {
    pub page: u32,
    /// e.g. "C0 M100 Y0 K0", "White (no ink)", "PANTONE 186 C 100%".
    pub colour: String,
    /// "text", "fill" or "line".
    pub kind: String,
}

/// A colour as far as the check needs to know it.
#[derive(Clone, Debug)]
pub(crate) enum Colour {
    Cmyk([f64; 4]),
    Gray(f64),
    Rgb([f64; 3]),
    /// Spot / DeviceN colorant names with their tints.
    Named(Vec<(String, f64)>),
    Unknown,
}

impl Colour {
    /// White: no ink at all, RGB white, or a white spot colour.
    fn is_white(&self) -> bool {
        match self {
            Colour::Cmyk(v) => v.iter().all(|c| *c < 0.005),
            Colour::Gray(g) => *g > 0.995,
            Colour::Rgb(v) => v.iter().all(|c| *c > 0.995),
            Colour::Named(inks) => {
                inks.iter().all(|(_, t)| *t < 0.005)
                    || inks.iter().any(|(n, t)| *t > 0.005 && n.to_lowercase().contains("white"))
            }
            Colour::Unknown => false,
        }
    }

    /// Black ink only (any tint of K, or grey).
    #[cfg(test)]
    fn is_black(&self) -> bool {
        match self {
            Colour::Cmyk([c, m, y, k]) => *c < 0.005 && *m < 0.005 && *y < 0.005 && *k > 0.005,
            Colour::Gray(g) => *g < 0.995,
            Colour::Named(inks) => {
                !inks.is_empty() && inks.iter().all(|(n, t)| n == "Black" || *t < 0.005) && inks.iter().any(|(_, t)| *t > 0.005)
            }
            Colour::Rgb(_) | Colour::Unknown => false,
        }
    }

    fn describe(&self) -> Option<String> {
        let pct = |v: f64| (v * 100.0).round() as i64;
        Some(match self {
            Colour::Cmyk([c, m, y, k]) if *c + *m + *y + *k < 0.005 => "White (no ink)".into(),
            Colour::Cmyk([c, m, y, k]) => format!("C{} M{} Y{} K{}", pct(*c), pct(*m), pct(*y), pct(*k)),
            Colour::Gray(_) => "White (no ink)".into(), // only non-black grey is white
            Colour::Rgb([r, g, b]) => format!(
                "RGB {} {} {}",
                (r * 255.0).round() as i64,
                (g * 255.0).round() as i64,
                (b * 255.0).round() as i64
            ),
            Colour::Named(inks) => {
                let used: Vec<String> =
                    inks.iter().filter(|(_, t)| *t > 0.005).map(|(n, t)| format!("{n} {}%", pct(*t))).collect();
                if used.is_empty() {
                    "White (no ink)".into()
                } else {
                    used.join(" + ")
                }
            }
            Colour::Unknown => return None, // patterns etc: can't judge, don't warn
        })
    }
}

/// The colour space currently selected for fill or stroke.
#[derive(Clone, Debug)]
pub(crate) enum Space {
    Gray,
    Rgb,
    Cmyk,
    Named(Vec<String>),
    Other,
}

fn nums(ops: &[Object]) -> Vec<f64> {
    ops.iter()
        .filter_map(|o| match o {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(*r as f64),
            _ => None,
        })
        .collect()
}

pub(crate) fn resolve_space(doc: &Document, cs: &Object, resources: &[&Dictionary], depth: u32) -> Space {
    if depth > 4 {
        return Space::Other;
    }
    match deref(doc, cs) {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"G" => Space::Gray,
            b"DeviceRGB" | b"RGB" => Space::Rgb,
            b"DeviceCMYK" | b"CMYK" => Space::Cmyk,
            b"Pattern" => Space::Other,
            other => find_named(doc, resources, b"ColorSpace", other)
                .map(|o| resolve_space(doc, o, resources, depth + 1))
                .unwrap_or(Space::Other),
        },
        Object::Array(arr) => {
            let family = arr.first().and_then(|o| deref(doc, o).as_name().ok()).unwrap_or(b"");
            match family {
                b"CalGray" => Space::Gray,
                b"CalRGB" => Space::Rgb,
                b"ICCBased" => match arr
                    .get(1)
                    .and_then(|o| deref(doc, o).as_stream().ok())
                    .and_then(|s| s.dict.get(b"N").ok().and_then(|n| n.as_i64().ok()))
                {
                    Some(1) => Space::Gray,
                    Some(3) => Space::Rgb,
                    Some(4) => Space::Cmyk,
                    _ => Space::Other,
                },
                b"Separation" => match arr.get(1).map(|o| deref(doc, o).as_name()) {
                    Some(Ok(n)) => Space::Named(vec![String::from_utf8_lossy(n).into_owned()]),
                    _ => Space::Other,
                },
                b"DeviceN" => match arr.get(1).map(|o| deref(doc, o)) {
                    Some(Object::Array(names)) => Space::Named(
                        names.iter().filter_map(|o| o.as_name().ok()).map(|n| String::from_utf8_lossy(n).into_owned()).collect(),
                    ),
                    _ => Space::Other,
                },
                _ => Space::Other,
            }
        }
        _ => Space::Other,
    }
}

/// Colour from `sc`/`scn` operands in the current space.
pub(crate) fn colour_in(space: &Space, v: &[f64]) -> Colour {
    match space {
        Space::Gray if v.len() == 1 => Colour::Gray(v[0]),
        Space::Rgb if v.len() == 3 => Colour::Rgb([v[0], v[1], v[2]]),
        Space::Cmyk if v.len() == 4 => Colour::Cmyk([v[0], v[1], v[2], v[3]]),
        Space::Named(names) if v.len() == names.len() => {
            Colour::Named(names.iter().cloned().zip(v.iter().copied()).collect())
        }
        _ => Colour::Unknown,
    }
}

/// Initial colour when a space is selected with cs/CS (all components 0,
/// except grey/CMYK/RGB black and full-tint spots per the PDF spec).
pub(crate) fn initial(space: &Space) -> Colour {
    match space {
        Space::Gray => Colour::Gray(0.0),
        Space::Rgb => Colour::Rgb([0.0; 3]),
        Space::Cmyk => Colour::Cmyk([0.0, 0.0, 0.0, 1.0]),
        Space::Named(names) => Colour::Named(names.iter().map(|n| (n.clone(), 1.0)).collect()),
        Space::Other => Colour::Unknown,
    }
}

#[derive(Clone)]
struct State {
    fill_space: Space,
    stroke_space: Space,
    fill: Colour,
    stroke: Colour,
    op_fill: bool,
    op_stroke: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            fill_space: Space::Gray,
            stroke_space: Space::Gray,
            fill: Colour::Gray(0.0),
            stroke: Colour::Gray(0.0),
            op_fill: false,
            op_stroke: false,
        }
    }
}

fn record(out: &mut BTreeSet<OverprintHit>, page: u32, colour: &Colour, kind: &str) {
    if !colour.is_white() {
        return;
    }
    if let Some(desc) = colour.describe() {
        out.insert(OverprintHit { page, colour: desc, kind: kind.into() });
    }
}

fn walk(
    doc: &Document,
    content: &[u8],
    resources: &[&Dictionary],
    start: State,
    page: u32,
    depth: u32,
    out: &mut BTreeSet<OverprintHit>,
) {
    let Ok(ops) = Content::decode(content) else { return };
    let mut st = start;
    let mut stack: Vec<State> = Vec::new();
    for op in &ops.operations {
        let v = nums(&op.operands);
        let first = op.operands.first();
        match op.operator.as_str() {
            "q" => stack.push(st.clone()),
            "Q" => {
                if let Some(s) = stack.pop() {
                    st = s;
                }
            }
            "g" if v.len() == 1 => (st.fill_space, st.fill) = (Space::Gray, Colour::Gray(v[0])),
            "G" if v.len() == 1 => (st.stroke_space, st.stroke) = (Space::Gray, Colour::Gray(v[0])),
            "rg" if v.len() == 3 => (st.fill_space, st.fill) = (Space::Rgb, Colour::Rgb([v[0], v[1], v[2]])),
            "RG" if v.len() == 3 => (st.stroke_space, st.stroke) = (Space::Rgb, Colour::Rgb([v[0], v[1], v[2]])),
            "k" if v.len() == 4 => (st.fill_space, st.fill) = (Space::Cmyk, Colour::Cmyk([v[0], v[1], v[2], v[3]])),
            "K" if v.len() == 4 => (st.stroke_space, st.stroke) = (Space::Cmyk, Colour::Cmyk([v[0], v[1], v[2], v[3]])),
            "cs" => {
                if let Some(o) = first {
                    st.fill_space = resolve_space(doc, o, resources, 0);
                    st.fill = initial(&st.fill_space);
                }
            }
            "CS" => {
                if let Some(o) = first {
                    st.stroke_space = resolve_space(doc, o, resources, 0);
                    st.stroke = initial(&st.stroke_space);
                }
            }
            "sc" | "scn" => st.fill = colour_in(&st.fill_space, &v),
            "SC" | "SCN" => st.stroke = colour_in(&st.stroke_space, &v),
            "gs" => {
                let Some(Ok(name)) = first.map(|o| o.as_name()) else { continue };
                let Some(Object::Dictionary(egs)) = find_named(doc, resources, b"ExtGState", name) else { continue };
                let get = |k: &[u8]| egs.get(k).ok().and_then(|o| deref(doc, o).as_bool().ok());
                if let Some(op_upper) = get(b"OP") {
                    st.op_stroke = op_upper;
                    // /OP also sets fill overprint unless /op is given.
                    if get(b"op").is_none() {
                        st.op_fill = op_upper;
                    }
                }
                if let Some(op_lower) = get(b"op") {
                    st.op_fill = op_lower;
                }
            }
            "f" | "F" | "f*" => {
                if st.op_fill {
                    record(out, page, &st.fill, "fill");
                }
            }
            "S" | "s" => {
                if st.op_stroke {
                    record(out, page, &st.stroke, "line");
                }
            }
            "B" | "B*" | "b" | "b*" => {
                if st.op_fill {
                    record(out, page, &st.fill, "fill");
                }
                if st.op_stroke {
                    record(out, page, &st.stroke, "line");
                }
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if st.op_fill {
                    record(out, page, &st.fill, "text");
                }
            }
            "Do" if depth < MAX_FORM_DEPTH => {
                let Some(Ok(name)) = first.map(|o| o.as_name()) else { continue };
                let Some(Object::Stream(stream)) = find_named(doc, resources, b"XObject", name) else { continue };
                if stream.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Form".as_slice()) {
                    continue;
                }
                let mut form_res: Vec<&Dictionary> = Vec::new();
                if let Ok(Object::Dictionary(d)) = stream.dict.get_deref(b"Resources", doc) {
                    form_res.push(d);
                }
                form_res.extend_from_slice(resources);
                let data = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
                walk(doc, &data, &form_res, st.clone(), page, depth + 1, out);
            }
            _ => {}
        }
    }
}

/// Every white object set to overprint, per page (deduplicated).
pub fn check_overprint(path: &Path) -> Result<Vec<OverprintHit>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let mut out = BTreeSet::new();
    for (num, page_id) in doc.get_pages() {
        let content = doc.get_page_content(page_id);
        let (own, inherited) = doc.get_page_resources(page_id).map_err(|e| format!("Could not read page resources: {e}"))?;
        let mut resources: Vec<&Dictionary> = own.into_iter().collect();
        resources.extend(inherited.iter().filter_map(|id| doc.get_dictionary(*id).ok()));
        walk(&doc, &content, &resources, State::default(), num, 0, &mut out);
    }
    Ok(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MP_TEST_PDF=path cargo test overprint_file -- --nocapture`
    #[test]
    fn overprint_file() {
        let Ok(p) = std::env::var("MP_TEST_PDF") else { return };
        println!("{:?}", check_overprint(Path::new(&p)));
    }

    #[test]
    fn black_rules() {
        assert!(Colour::Cmyk([0.0, 0.0, 0.0, 1.0]).is_black());
        assert!(Colour::Cmyk([0.0, 0.0, 0.0, 0.4]).is_black());
        assert!(!Colour::Cmyk([0.6, 0.4, 0.4, 1.0]).is_black()); // rich black
        assert!(!Colour::Cmyk([0.0, 0.0, 0.0, 0.0]).is_black()); // white
        assert!(Colour::Gray(0.0).is_black());
        assert!(!Colour::Gray(1.0).is_black());
        assert!(!Colour::Rgb([0.0, 0.0, 0.0]).is_black());
    }

    #[test]
    fn white_rules() {
        assert!(Colour::Cmyk([0.0; 4]).is_white());
        assert!(Colour::Gray(1.0).is_white());
        assert!(Colour::Rgb([1.0; 3]).is_white());
        assert!(Colour::Named(vec![("PANTONE 186 C".into(), 0.0)]).is_white());
        assert!(Colour::Named(vec![("White".into(), 1.0)]).is_white());
        assert!(!Colour::Named(vec![("PANTONE 186 C".into(), 1.0)]).is_white());
        assert!(!Colour::Cmyk([1.0, 0.0, 0.0, 0.0]).is_white());
        assert!(!Colour::Rgb([1.0, 0.0, 0.0]).is_white());
    }
}
