//! Recolour tool: finds the object under a click (paths and text, also
//! inside form XObjects), reports its fill and stroke colours, finds every
//! object sharing them, and repaints the chosen objects in CMYK.
//!
//! Edits are spliced into the content streams as bytes, so everything else
//! in a stream (inline images, marked content, odd syntax) is left exactly
//! as it was. A path is wrapped as `q <new colour> … paint Q`; text (where
//! `q` isn't allowed) gets the new colour before the show operator and the
//! old colour put back straight after it.

use crate::colorcheck::deref;
use crate::imageres::{mul, Matrix, IDENTITY};
use crate::overprint::{colour_in, initial, resolve_space, Colour, Space};
use crate::pagebox;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

const MAX_FORM_DEPTH: u32 = 8;

// ---------------------------------------------------------------------------
// Content stream lexer (keeps byte offsets so edits can be spliced in)

#[derive(Clone, Debug)]
enum Tok {
    Num(f64),
    Name(Vec<u8>),
    Str(Vec<u8>),
    Arr(Vec<Tok>),
    Other,
}

#[derive(Debug)]
struct Op {
    name: String,
    args: Vec<Tok>,
    /// Byte range of the whole operation (first operand to operator).
    start: usize,
    end: usize,
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

struct Lexer<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Lexer<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.d.len() {
            let b = self.d[self.i];
            if is_ws(b) {
                self.i += 1;
            } else if b == b'%' {
                while self.i < self.d.len() && self.d[self.i] != b'\n' && self.d[self.i] != b'\r' {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }

    fn word(&mut self) -> &'a [u8] {
        let s = self.i;
        while self.i < self.d.len() && !is_ws(self.d[self.i]) && !is_delim(self.d[self.i]) {
            self.i += 1;
        }
        &self.d[s..self.i]
    }

    fn literal_string(&mut self) -> Vec<u8> {
        // at '('
        self.i += 1;
        let mut out = Vec::new();
        let mut depth = 1;
        while self.i < self.d.len() {
            let b = self.d[self.i];
            self.i += 1;
            match b {
                b'\\' => {
                    let Some(&e) = self.d.get(self.i) else { break };
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.d.get(self.i) {
                                    Some(&c @ b'0'..=b'7') => {
                                        v = v * 8 + (c - b'0') as u32;
                                        self.i += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        b'\r' => {
                            if self.d.get(self.i) == Some(&b'\n') {
                                self.i += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                _ => out.push(b),
            }
        }
        out
    }

    fn hex_string(&mut self) -> Vec<u8> {
        // at '<'
        self.i += 1;
        let mut nibbles = Vec::new();
        while self.i < self.d.len() && self.d[self.i] != b'>' {
            let b = self.d[self.i];
            if let Some(v) = (b as char).to_digit(16) {
                nibbles.push(v as u8);
            }
            self.i += 1;
        }
        self.i += 1;
        if nibbles.len() % 2 == 1 {
            nibbles.push(0);
        }
        nibbles.chunks(2).map(|c| c[0] * 16 + c[1]).collect()
    }

    /// One operand, or None at an operator / end of data.
    fn operand(&mut self) -> Option<Tok> {
        self.skip_ws();
        let b = *self.d.get(self.i)?;
        Some(match b {
            b'/' => {
                self.i += 1;
                let raw = self.word();
                let mut name = Vec::with_capacity(raw.len());
                let mut k = 0;
                while k < raw.len() {
                    if raw[k] == b'#' && k + 2 < raw.len() {
                        if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&raw[k + 1..k + 3]).unwrap_or("x"), 16) {
                            name.push(v);
                            k += 3;
                            continue;
                        }
                    }
                    name.push(raw[k]);
                    k += 1;
                }
                Tok::Name(name)
            }
            b'(' => Tok::Str(self.literal_string()),
            b'<' if self.d.get(self.i + 1) == Some(&b'<') => {
                self.i += 2;
                // Skip a dictionary (only used by BDC/DP and inline images).
                loop {
                    self.skip_ws();
                    match self.d.get(self.i) {
                        None => break,
                        Some(b'>') if self.d.get(self.i + 1) == Some(&b'>') => {
                            self.i += 2;
                            break;
                        }
                        _ => {
                            if self.operand().is_none() {
                                // A bare word inside a dict (true/false/null).
                                let w = self.word();
                                if w.is_empty() {
                                    self.i += 1;
                                }
                            }
                        }
                    }
                }
                Tok::Other
            }
            b'<' => Tok::Str(self.hex_string()),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_ws();
                    match self.d.get(self.i) {
                        None => break,
                        Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        _ => match self.operand() {
                            Some(t) => items.push(t),
                            None => {
                                let w = self.word();
                                if w.is_empty() {
                                    self.i += 1;
                                }
                                items.push(Tok::Other);
                            }
                        },
                    }
                }
                Tok::Arr(items)
            }
            b'{' | b'}' | b')' | b'>' | b']' => {
                self.i += 1;
                Tok::Other
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => {
                let w = self.word();
                std::str::from_utf8(w).ok().and_then(|s| s.parse::<f64>().ok()).map(Tok::Num).unwrap_or(Tok::Other)
            }
            _ => {
                // A keyword: true/false/null are operands, anything else is
                // the operator, which the caller reads.
                let save = self.i;
                let w = self.word();
                if w == b"true" || w == b"false" || w == b"null" {
                    Tok::Other
                } else {
                    self.i = save;
                    return None;
                }
            }
        })
    }

    /// Skips inline image data after `ID`, up to and including `EI`.
    fn skip_inline_image(&mut self) {
        // Find the ID keyword.
        while self.i + 1 < self.d.len() {
            if self.d[self.i] == b'I'
                && self.d[self.i + 1] == b'D'
                && (self.i == 0 || is_ws(self.d[self.i - 1]) || is_delim(self.d[self.i - 1]))
                && self.d.get(self.i + 2).map_or(true, |b| is_ws(*b))
            {
                self.i += 3; // "ID" + one whitespace byte
                break;
            }
            self.i += 1;
        }
        while self.i + 2 < self.d.len() {
            if is_ws(self.d[self.i])
                && self.d[self.i + 1] == b'E'
                && self.d[self.i + 2] == b'I'
                && self.d.get(self.i + 3).map_or(true, |b| is_ws(*b) || is_delim(*b))
            {
                self.i += 3;
                return;
            }
            self.i += 1;
        }
        self.i = self.d.len();
    }
}

fn lex(data: &[u8]) -> Vec<Op> {
    let mut lx = Lexer { d: data, i: 0 };
    let mut ops = Vec::new();
    let mut args = Vec::new();
    let mut start: Option<usize> = None;
    loop {
        lx.skip_ws();
        if lx.i >= data.len() {
            break;
        }
        let here = lx.i;
        if let Some(t) = lx.operand() {
            start.get_or_insert(here);
            args.push(t);
            continue;
        }
        let w = lx.word();
        if w.is_empty() {
            lx.i += 1; // stray delimiter
            continue;
        }
        let name = String::from_utf8_lossy(w).into_owned();
        if name == "BI" {
            lx.skip_inline_image();
        }
        ops.push(Op { name, args: std::mem::take(&mut args), start: start.take().unwrap_or(here), end: lx.i });
    }
    ops
}

fn nums(args: &[Tok]) -> Vec<f64> {
    args.iter().filter_map(|t| if let Tok::Num(n) = t { Some(*n) } else { None }).collect()
}

// ---------------------------------------------------------------------------
// Colours

/// A colour as a stable string, used to match "same colour" objects.
fn colour_key(c: &Colour) -> Option<String> {
    let r = |v: f64| format!("{:.3}", v.clamp(0.0, 1.0));
    Some(match c {
        Colour::Cmyk(v) => format!("cmyk:{}", v.iter().map(|x| r(*x)).collect::<Vec<_>>().join(",")),
        Colour::Gray(g) => format!("gray:{}", r(*g)),
        Colour::Rgb(v) => format!("rgb:{}", v.iter().map(|x| r(*x)).collect::<Vec<_>>().join(",")),
        Colour::Named(inks) => format!(
            "named:{}",
            inks.iter().map(|(n, t)| format!("{n}={}", r(*t))).collect::<Vec<_>>().join("|")
        ),
        Colour::Unknown => return None,
    })
}

#[derive(Serialize, Clone, Debug)]
pub struct ColourInfo {
    /// e.g. "C0 M100 Y100 K0", "PANTONE 361 C 100%", "RGB 255 0 0".
    pub label: String,
    /// Display colour for process colours; spot swatches come from the UI.
    pub hex: Option<String>,
    /// Spot / DeviceN ink names (empty for process colours).
    pub inks: Vec<String>,
    /// Starting values for the CMYK editor (0–100).
    pub cmyk: [f64; 4],
}

fn hex(r: f64, g: f64, b: f64) -> String {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
}

fn colour_info(c: &Colour) -> Option<ColourInfo> {
    let pct = |v: f64| (v * 100.0).round();
    Some(match c {
        Colour::Cmyk([cc, m, y, k]) => ColourInfo {
            label: format!("C{} M{} Y{} K{}", pct(*cc), pct(*m), pct(*y), pct(*k)),
            hex: Some(hex((1.0 - cc) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k))),
            inks: vec![],
            cmyk: [pct(*cc), pct(*m), pct(*y), pct(*k)],
        },
        Colour::Gray(g) => ColourInfo {
            label: format!("Grey K{}", pct(1.0 - g)),
            hex: Some(hex(*g, *g, *g)),
            inks: vec![],
            cmyk: [0.0, 0.0, 0.0, pct(1.0 - g)],
        },
        Colour::Rgb([r, g, b]) => {
            let k = 1.0 - r.max(*g).max(*b);
            let d = (1.0 - k).max(1e-6);
            ColourInfo {
                label: format!(
                    "RGB {} {} {}",
                    (r * 255.0).round(),
                    (g * 255.0).round(),
                    (b * 255.0).round()
                ),
                hex: Some(hex(*r, *g, *b)),
                inks: vec![],
                cmyk: [pct((1.0 - r - k) / d), pct((1.0 - g - k) / d), pct((1.0 - b - k) / d), pct(k)],
            }
        }
        Colour::Named(inks) => {
            let mut cmyk = [0.0; 4];
            for (n, t) in inks {
                if let Some(i) = ["Cyan", "Magenta", "Yellow", "Black"].iter().position(|p| p == n) {
                    cmyk[i] = pct(*t);
                }
            }
            ColourInfo {
                label: inks.iter().map(|(n, t)| format!("{n} {}%", pct(*t))).collect::<Vec<_>>().join(" + "),
                hex: None,
                inks: inks.iter().map(|(n, _)| n.clone()).collect(),
                cmyk,
            }
        }
        Colour::Unknown => return None,
    })
}

// ---------------------------------------------------------------------------
// Fonts (just enough to size text for hit-testing)

struct Font {
    two_byte: bool,
    widths: HashMap<u32, f64>,
    default_w: f64,
    /// Glyph space → text space (1/1000 for everything but Type 3).
    scale: f64,
}

fn num_obj(doc: &Document, o: &Object) -> Option<f64> {
    match deref(doc, o) {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn load_font(doc: &Document, font: &Dictionary) -> Font {
    let subtype = font.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"");
    let mut widths = HashMap::new();
    if subtype == b"Type0" {
        let mut default_w = 1000.0;
        let desc = font
            .get_deref(b"DescendantFonts", doc)
            .ok()
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| deref(doc, o).as_dict().ok());
        if let Some(desc) = desc {
            if let Some(dw) = desc.get(b"DW").ok().and_then(|o| num_obj(doc, o)) {
                default_w = dw;
            }
            if let Ok(Object::Array(w)) = desc.get_deref(b"W", doc) {
                let mut k = 0;
                while k < w.len() {
                    let Some(first) = num_obj(doc, &w[k]) else { break };
                    match w.get(k + 1).map(|o| deref(doc, o)) {
                        Some(Object::Array(list)) => {
                            for (j, v) in list.iter().enumerate() {
                                if let Some(v) = num_obj(doc, v) {
                                    widths.insert(first as u32 + j as u32, v);
                                }
                            }
                            k += 2;
                        }
                        Some(_) => {
                            let last = w.get(k + 1).and_then(|o| num_obj(doc, o)).unwrap_or(first);
                            let v = w.get(k + 2).and_then(|o| num_obj(doc, o)).unwrap_or(default_w);
                            for c in first as u32..=(last as u32).min(first as u32 + 65535) {
                                widths.insert(c, v);
                            }
                            k += 3;
                        }
                        None => break,
                    }
                }
            }
        }
        return Font { two_byte: true, widths, default_w, scale: 0.001 };
    }
    let first = font.get(b"FirstChar").ok().and_then(|o| num_obj(doc, o)).unwrap_or(0.0) as u32;
    if let Ok(Object::Array(w)) = font.get_deref(b"Widths", doc) {
        for (j, v) in w.iter().enumerate() {
            if let Some(v) = num_obj(doc, v) {
                widths.insert(first + j as u32, v);
            }
        }
    }
    let scale = if subtype == b"Type3" {
        font.get_deref(b"FontMatrix", doc)
            .ok()
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| num_obj(doc, o))
            .unwrap_or(0.001)
    } else {
        0.001
    };
    let default_w = if subtype == b"Type3" { 0.0 } else { 500.0 };
    Font { two_byte: false, widths, default_w, scale }
}

// ---------------------------------------------------------------------------
// Walking the page

/// What a paint operation looks like, for hit-testing.
#[derive(Clone, Debug)]
enum Shape {
    /// Subpaths in page space; `even_odd` fill rule; stroke half-width.
    Path { subpaths: Vec<Vec<(f64, f64)>>, fill: bool, stroke: bool, half_width: f64 },
    /// A text run's quad in page space.
    Quad([(f64, f64); 4]),
}

#[derive(Clone, Debug)]
struct Paint {
    stream: usize,
    /// Where `pre` goes (start of the path, or the text show operator).
    start: usize,
    /// Where `post` goes (end of the paint operator).
    end: usize,
    /// The object can be wrapped in q/Q (paths without a clip).
    wrap: bool,
    fills: bool,
    strokes: bool,
    fill: Colour,
    stroke: Colour,
    fill_restore: Vec<u8>,
    stroke_restore: Vec<u8>,
    shape: Shape,
}

impl Paint {
    fn bbox(&self) -> Option<[f64; 4]> {
        let pts: Vec<(f64, f64)> = match &self.shape {
            Shape::Path { subpaths, .. } => subpaths.iter().flatten().copied().collect(),
            Shape::Quad(q) => q.to_vec(),
        };
        if pts.is_empty() {
            return None;
        }
        let pad = match &self.shape {
            Shape::Path { stroke: true, half_width, .. } => *half_width,
            _ => 0.0,
        };
        let x0 = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min) - pad;
        let x1 = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max) + pad;
        let y0 = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min) - pad;
        let y1 = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max) + pad;
        Some([x0, y0, x1, y1])
    }

    fn hit(&self, x: f64, y: f64, tol: f64) -> bool {
        match &self.shape {
            Shape::Quad(q) => winding(&[q.to_vec()], x, y) != 0,
            Shape::Path { subpaths, fill, stroke, half_width } => {
                if *fill && winding(subpaths, x, y) != 0 {
                    return true;
                }
                if *stroke || subpaths.iter().all(|s| s.len() < 3) {
                    let reach = half_width.max(tol);
                    for sp in subpaths {
                        for w in sp.windows(2) {
                            if seg_dist(w[0], w[1], (x, y)) <= reach {
                                return true;
                            }
                        }
                    }
                }
                false
            }
        }
    }
}

/// Non-zero winding number of the point (subpaths implicitly closed).
fn winding(subpaths: &[Vec<(f64, f64)>], x: f64, y: f64) -> i32 {
    let mut wn = 0;
    for sp in subpaths {
        let n = sp.len();
        if n < 3 {
            continue;
        }
        for i in 0..n {
            let (a, b) = (sp[i], sp[(i + 1) % n]);
            if a.1 <= y {
                if b.1 > y && cross(a, b, (x, y)) > 0.0 {
                    wn += 1;
                }
            } else if b.1 <= y && cross(a, b, (x, y)) < 0.0 {
                wn -= 1;
            }
        }
    }
    wn
}

fn cross(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    (b.0 - a.0) * (p.1 - a.1) - (p.0 - a.0) * (b.1 - a.1)
}

fn seg_dist(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    ((a.0 + t * dx - p.0).powi(2) + (a.1 + t * dy - p.1).powi(2)).sqrt()
}

fn apply(m: &Matrix, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// Which stream a paint lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum StreamId {
    Page(ObjectId),
    Form(ObjectId),
}

#[derive(Clone)]
struct Gfx {
    ctm: Matrix,
    fill_space: Space,
    stroke_space: Space,
    fill: Colour,
    stroke: Colour,
    /// Bytes that set the current fill / stroke colour again.
    fill_cs: Vec<u8>,
    fill_val: Option<(Vec<u8>, bool)>, // (op bytes, op also sets the space)
    stroke_cs: Vec<u8>,
    stroke_val: Option<(Vec<u8>, bool)>,
    line_width: f64,
    font: Option<Vec<u8>>,
    font_size: f64,
    char_spacing: f64,
    word_spacing: f64,
    h_scale: f64,
    leading: f64,
    rise: f64,
    render: i64,
}

impl Default for Gfx {
    fn default() -> Self {
        Gfx {
            ctm: IDENTITY,
            fill_space: Space::Gray,
            stroke_space: Space::Gray,
            fill: Colour::Gray(0.0),
            stroke: Colour::Gray(0.0),
            fill_cs: b"/DeviceGray cs".to_vec(),
            fill_val: None,
            stroke_cs: b"/DeviceGray CS".to_vec(),
            stroke_val: None,
            line_width: 1.0,
            font: None,
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            h_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            render: 0,
        }
    }
}

fn restore(cs: &[u8], val: &Option<(Vec<u8>, bool)>) -> Vec<u8> {
    match val {
        Some((op, true)) => op.clone(),
        Some((op, false)) => [cs, b" ", op].concat(),
        None => cs.to_vec(),
    }
}

struct Walker<'a> {
    doc: &'a Document,
    streams: Vec<StreamId>,
    paints: Vec<Paint>,
    fonts: HashMap<ObjectId, std::rc::Rc<Font>>,
    /// Paint geometry is only needed for the page being clicked on.
    geometry: bool,
}

impl<'a> Walker<'a> {
    fn stream_index(&mut self, id: StreamId) -> usize {
        if let Some(i) = self.streams.iter().position(|s| *s == id) {
            return i;
        }
        self.streams.push(id);
        self.streams.len() - 1
    }

    fn font(&mut self, resources: &[&Dictionary], name: &[u8]) -> Option<std::rc::Rc<Font>> {
        for res in resources {
            let Ok(Object::Dictionary(fonts)) = res.get_deref(b"Font", self.doc) else { continue };
            let Ok(entry) = fonts.get(name) else { continue };
            let id = entry.as_reference().ok();
            if let Some(id) = id {
                if let Some(f) = self.fonts.get(&id) {
                    return Some(f.clone());
                }
            }
            let dict = deref(self.doc, entry).as_dict().ok()?;
            let f = std::rc::Rc::new(load_font(self.doc, dict));
            if let Some(id) = id {
                self.fonts.insert(id, f.clone());
            }
            return Some(f);
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(&mut self, data: &[u8], resources: &[&Dictionary], stream: StreamId, start: Gfx, depth: u32) {
        let doc = self.doc;
        let sidx = self.stream_index(stream);
        let ops = lex(data);
        let mut g = start;
        let mut stack: Vec<Gfx> = Vec::new();
        // Current path.
        let mut path_start: Option<usize> = None;
        let mut clip_in_path = false;
        let mut subpaths: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut cur: Vec<(f64, f64)> = Vec::new();
        let mut pen = (0.0, 0.0);
        // Text state.
        let mut tm = IDENTITY;
        let mut tlm = IDENTITY;
        let mut font: Option<std::rc::Rc<Font>> = None;

        for op in &ops {
            let v = nums(&op.args);
            let bytes = &data[op.start..op.end];
            match op.name.as_str() {
                "q" => stack.push(g.clone()),
                "Q" => {
                    if let Some(s) = stack.pop() {
                        g = s;
                    }
                }
                "cm" if v.len() == 6 => g.ctm = mul(&[v[0], v[1], v[2], v[3], v[4], v[5]], &g.ctm),
                "w" if v.len() == 1 => g.line_width = v[0],
                "g" | "rg" | "k" | "G" | "RG" | "K" => {
                    let (space, colour, cs) = match (op.name.as_str(), v.len()) {
                        ("g" | "G", 1) => (Space::Gray, Colour::Gray(v[0]), "/DeviceGray"),
                        ("rg" | "RG", 3) => (Space::Rgb, Colour::Rgb([v[0], v[1], v[2]]), "/DeviceRGB"),
                        ("k" | "K", 4) => (Space::Cmyk, Colour::Cmyk([v[0], v[1], v[2], v[3]]), "/DeviceCMYK"),
                        _ => continue,
                    };
                    if op.name.chars().next().unwrap().is_lowercase() {
                        g.fill_space = space;
                        g.fill = colour;
                        g.fill_cs = format!("{cs} cs").into_bytes();
                        g.fill_val = Some((bytes.to_vec(), true));
                    } else {
                        g.stroke_space = space;
                        g.stroke = colour;
                        g.stroke_cs = format!("{cs} CS").into_bytes();
                        g.stroke_val = Some((bytes.to_vec(), true));
                    }
                }
                "cs" | "CS" => {
                    let Some(Tok::Name(n)) = op.args.first() else { continue };
                    let space = resolve_space(doc, &Object::Name(n.clone()), resources, 0);
                    if op.name == "cs" {
                        g.fill = initial(&space);
                        g.fill_space = space;
                        g.fill_cs = bytes.to_vec();
                        g.fill_val = None;
                    } else {
                        g.stroke = initial(&space);
                        g.stroke_space = space;
                        g.stroke_cs = bytes.to_vec();
                        g.stroke_val = None;
                    }
                }
                "sc" | "scn" => {
                    g.fill = if op.args.iter().any(|t| matches!(t, Tok::Name(_))) { Colour::Unknown } else { colour_in(&g.fill_space, &v) };
                    g.fill_val = Some((bytes.to_vec(), false));
                }
                "SC" | "SCN" => {
                    g.stroke = if op.args.iter().any(|t| matches!(t, Tok::Name(_))) { Colour::Unknown } else { colour_in(&g.stroke_space, &v) };
                    g.stroke_val = Some((bytes.to_vec(), false));
                }

                // Path construction.
                "m" if v.len() == 2 => {
                    path_start.get_or_insert(op.start);
                    if cur.len() > 1 {
                        subpaths.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                    pen = (v[0], v[1]);
                    cur.push(apply(&g.ctm, v[0], v[1]));
                }
                "l" if v.len() == 2 => {
                    path_start.get_or_insert(op.start);
                    pen = (v[0], v[1]);
                    cur.push(apply(&g.ctm, v[0], v[1]));
                }
                "c" | "v" | "y" => {
                    path_start.get_or_insert(op.start);
                    let (p1, p2, p3) = match (op.name.as_str(), v.len()) {
                        ("c", 6) => ((v[0], v[1]), (v[2], v[3]), (v[4], v[5])),
                        ("v", 4) => (pen, (v[0], v[1]), (v[2], v[3])),
                        ("y", 4) => ((v[0], v[1]), (v[2], v[3]), (v[2], v[3])),
                        _ => continue,
                    };
                    let p0 = pen;
                    for s in 1..=8 {
                        let t = s as f64 / 8.0;
                        let u = 1.0 - t;
                        let x = u * u * u * p0.0 + 3.0 * u * u * t * p1.0 + 3.0 * u * t * t * p2.0 + t * t * t * p3.0;
                        let y = u * u * u * p0.1 + 3.0 * u * u * t * p1.1 + 3.0 * u * t * t * p2.1 + t * t * t * p3.1;
                        cur.push(apply(&g.ctm, x, y));
                    }
                    pen = p3;
                }
                "re" if v.len() == 4 => {
                    path_start.get_or_insert(op.start);
                    if cur.len() > 1 {
                        subpaths.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                    let (x, y, w, h) = (v[0], v[1], v[2], v[3]);
                    subpaths.push(vec![
                        apply(&g.ctm, x, y),
                        apply(&g.ctm, x + w, y),
                        apply(&g.ctm, x + w, y + h),
                        apply(&g.ctm, x, y + h),
                        apply(&g.ctm, x, y),
                    ]);
                    pen = (x, y);
                }
                "h" => {
                    if let Some(&f) = cur.first() {
                        cur.push(f);
                    }
                }
                "W" | "W*" => clip_in_path = true,
                "n" | "f" | "F" | "f*" | "S" | "s" | "B" | "B*" | "b" | "b*" => {
                    if matches!(op.name.as_str(), "s" | "b" | "b*") {
                        if let Some(&f) = cur.first() {
                            cur.push(f);
                        }
                    }
                    if cur.len() > 1 {
                        subpaths.push(std::mem::take(&mut cur));
                    }
                    let fills = matches!(op.name.as_str(), "f" | "F" | "f*" | "B" | "B*" | "b" | "b*");
                    let strokes = matches!(op.name.as_str(), "S" | "s" | "B" | "B*" | "b" | "b*");
                    if (fills || strokes) && path_start.is_some() {
                        let scale = (g.ctm[0] * g.ctm[3] - g.ctm[1] * g.ctm[2]).abs().sqrt();
                        let shape = if self.geometry {
                            Shape::Path {
                                subpaths: std::mem::take(&mut subpaths),
                                fill: fills,
                                stroke: strokes,
                                half_width: (g.line_width.max(0.0) * scale / 2.0).max(0.25),
                            }
                        } else {
                            Shape::Quad([(0.0, 0.0); 4])
                        };
                        self.paints.push(Paint {
                            stream: sidx,
                            start: path_start.unwrap(),
                            end: op.end,
                            wrap: !clip_in_path,
                            fills,
                            strokes,
                            fill: g.fill.clone(),
                            stroke: g.stroke.clone(),
                            fill_restore: restore(&g.fill_cs, &g.fill_val),
                            stroke_restore: restore(&g.stroke_cs, &g.stroke_val),
                            shape,
                        });
                    }
                    path_start = None;
                    clip_in_path = false;
                    subpaths.clear();
                    cur.clear();
                }

                // Text.
                "BT" => {
                    tm = IDENTITY;
                    tlm = IDENTITY;
                }
                "Tf" => {
                    if let (Some(Tok::Name(n)), Some(size)) = (op.args.first(), v.first()) {
                        g.font = Some(n.clone());
                        g.font_size = *size;
                        font = self.font(resources, n);
                    }
                }
                "Tc" if v.len() == 1 => g.char_spacing = v[0],
                "Tw" if v.len() == 1 => g.word_spacing = v[0],
                "Tz" if v.len() == 1 => g.h_scale = v[0] / 100.0,
                "TL" if v.len() == 1 => g.leading = v[0],
                "Ts" if v.len() == 1 => g.rise = v[0],
                "Tr" if v.len() == 1 => g.render = v[0] as i64,
                "Td" | "TD" if v.len() == 2 => {
                    if op.name == "TD" {
                        g.leading = -v[1];
                    }
                    tlm = mul(&[1.0, 0.0, 0.0, 1.0, v[0], v[1]], &tlm);
                    tm = tlm;
                }
                "Tm" if v.len() == 6 => {
                    tlm = [v[0], v[1], v[2], v[3], v[4], v[5]];
                    tm = tlm;
                }
                "T*" => {
                    tlm = mul(&[1.0, 0.0, 0.0, 1.0, 0.0, -g.leading], &tlm);
                    tm = tlm;
                }
                "Tj" | "TJ" | "'" | "\"" => {
                    if op.name == "'" || op.name == "\"" {
                        if op.name == "\"" && v.len() >= 2 {
                            g.word_spacing = v[0];
                            g.char_spacing = v[1];
                        }
                        tlm = mul(&[1.0, 0.0, 0.0, 1.0, 0.0, -g.leading], &tlm);
                        tm = tlm;
                    }
                    // Advance of the run in text space.
                    let mut adv = 0.0;
                    let add_string = |s: &[u8], adv: &mut f64| {
                        let f = font.as_deref();
                        let two = f.map_or(false, |f| f.two_byte);
                        let step = if two { 2 } else { 1 };
                        for chunk in s.chunks(step) {
                            let code = chunk.iter().fold(0u32, |a, b| a * 256 + *b as u32);
                            let w = f.map_or(500.0, |f| *f.widths.get(&code).unwrap_or(&f.default_w));
                            let scale = f.map_or(0.001, |f| f.scale);
                            let mut tx = w * scale * g.font_size + g.char_spacing;
                            if !two && code == 32 {
                                tx += g.word_spacing;
                            }
                            *adv += tx * g.h_scale;
                        }
                    };
                    for t in &op.args {
                        match t {
                            Tok::Str(s) => add_string(s, &mut adv),
                            Tok::Arr(items) => {
                                for it in items {
                                    match it {
                                        Tok::Str(s) => add_string(s, &mut adv),
                                        Tok::Num(n) => adv -= n / 1000.0 * g.font_size * g.h_scale,
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let fills = matches!(g.render, 0 | 2 | 4 | 6);
                    let strokes = matches!(g.render, 1 | 2 | 5 | 6);
                    if fills || strokes {
                        let m = mul(&tm, &g.ctm);
                        let (lo, hi) = (g.rise - 0.25 * g.font_size, g.rise + 0.9 * g.font_size);
                        let quad = [apply(&m, 0.0, lo), apply(&m, adv, lo), apply(&m, adv, hi), apply(&m, 0.0, hi)];
                        self.paints.push(Paint {
                            stream: sidx,
                            start: op.start,
                            end: op.end,
                            wrap: false,
                            fills,
                            strokes,
                            fill: g.fill.clone(),
                            stroke: g.stroke.clone(),
                            fill_restore: restore(&g.fill_cs, &g.fill_val),
                            stroke_restore: restore(&g.stroke_cs, &g.stroke_val),
                            shape: Shape::Quad(quad),
                        });
                    }
                    tm = mul(&[1.0, 0.0, 0.0, 1.0, adv, 0.0], &tm);
                }

                "Do" if depth < MAX_FORM_DEPTH => {
                    let Some(Tok::Name(name)) = op.args.first() else { continue };
                    // The form's object id, so it can be edited in place.
                    let mut found: Option<(ObjectId, &Stream)> = None;
                    for res in resources {
                        let Ok(Object::Dictionary(xobjs)) = res.get_deref(b"XObject", doc) else { continue };
                        let Ok(Object::Reference(id)) = xobjs.get(name) else { continue };
                        if let Ok(Object::Stream(s)) = doc.get_object(*id) {
                            found = Some((*id, s));
                        }
                        break;
                    }
                    let Some((id, form)) = found else { continue };
                    if form.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Form".as_slice()) {
                        continue;
                    }
                    let form_m = form
                        .dict
                        .get(b"Matrix")
                        .ok()
                        .and_then(|o| o.as_array().ok())
                        .and_then(|a| crate::imageres::matrix_from(a))
                        .unwrap_or(IDENTITY);
                    let mut form_res: Vec<&Dictionary> = Vec::new();
                    if let Ok(Object::Dictionary(d)) = form.dict.get_deref(b"Resources", doc) {
                        form_res.push(d);
                    }
                    form_res.extend_from_slice(resources);
                    let content = form.decompressed_content().unwrap_or_else(|_| form.content.clone());
                    let mut inner = g.clone();
                    inner.ctm = mul(&form_m, &g.ctm);
                    self.walk(&content, &form_res, StreamId::Form(id), inner, depth + 1);
                }
                _ => {}
            }
        }
    }
}

struct PageWalk {
    paints: Vec<Paint>,
    streams: Vec<StreamId>,
    /// Decoded data of every stream, by index (pages and forms).
    data: Vec<Vec<u8>>,
}

fn page_resources<'a>(doc: &'a Document, page_id: ObjectId) -> Vec<&'a Dictionary> {
    let Ok((own, inherited)) = doc.get_page_resources(page_id) else { return Vec::new() };
    let mut resources: Vec<&Dictionary> = own.into_iter().collect();
    resources.extend(inherited.iter().filter_map(|id| doc.get_dictionary(*id).ok()));
    resources
}

fn stream_data(doc: &Document, id: StreamId) -> Vec<u8> {
    match id {
        StreamId::Page(page_id) => doc.get_page_content(page_id),
        StreamId::Form(oid) => match doc.get_object(oid) {
            Ok(Object::Stream(s)) => s.decompressed_content().unwrap_or_else(|_| s.content.clone()),
            _ => Vec::new(),
        },
    }
}

fn walk_page(doc: &Document, page_id: ObjectId, geometry: bool) -> PageWalk {
    let resources = page_resources(doc, page_id);
    let content = doc.get_page_content(page_id);
    let mut w = Walker { doc, streams: Vec::new(), paints: Vec::new(), fonts: HashMap::new(), geometry };
    w.walk(&content, &resources, StreamId::Page(page_id), Gfx::default(), 0);
    let data = w.streams.iter().map(|s| stream_data(doc, *s)).collect();
    PageWalk { paints: w.paints, streams: w.streams, data }
}

// ---------------------------------------------------------------------------
// Commands

/// A picked object: the page and its position in that page's paint order.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PaintRef {
    pub page: u32,
    pub index: usize,
}

#[derive(Serialize)]
pub struct Picked {
    pub target: PaintRef,
    /// "shape" or "text".
    pub kind: String,
    pub fill: Option<ColourInfo>,
    pub stroke: Option<ColourInfo>,
    /// The object's outline as [left, top, width, height] of the displayed page.
    pub norm: [f64; 4],
}

struct PageGeom {
    media: [f64; 4],
    rotate: i32,
}

fn page_geom(doc: &Document, page_id: ObjectId) -> PageGeom {
    let (media, _) = pagebox::resolve_box(doc, page_id, "MediaBox", [0.0, 0.0, 612.0, 792.0]);
    PageGeom { media, rotate: pagebox::rotate_of(doc, page_id) }
}

impl PageGeom {
    /// Displayed fraction of the page → PDF user space.
    fn to_pdf(&self, x: f64, y: f64) -> (f64, f64) {
        let (ux, uy) = pagebox::rotate_point(x, y, (360 - self.rotate) % 360);
        let m = self.media;
        (m[0] + ux * (m[2] - m[0]), m[1] + (1.0 - uy) * (m[3] - m[1]))
    }

    fn norm(&self, bbox: [f64; 4]) -> [f64; 4] {
        pagebox::normalize_rect(bbox, self.media, self.rotate)
    }
}

/// The topmost recolourable object at a point (`x`, `y` fractions of the
/// displayed page, `tol` hit tolerance in points).
pub fn pick(path: &Path, page: u32, x: f64, y: f64, tol: f64) -> Result<Option<Picked>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = pagebox::page_id_for(&doc, page)?;
    let geom = page_geom(&doc, page_id);
    let (px, py) = geom.to_pdf(x, y);
    let walk = walk_page(&doc, page_id, true);
    for (index, p) in walk.paints.iter().enumerate().rev() {
        if !p.hit(px, py, tol) {
            continue;
        }
        let fill = if p.fills { colour_info(&p.fill) } else { None };
        let stroke = if p.strokes { colour_info(&p.stroke) } else { None };
        if fill.is_none() && stroke.is_none() {
            continue; // patterns and the like can't be recoloured
        }
        return Ok(Some(Picked {
            target: PaintRef { page, index },
            kind: if matches!(p.shape, Shape::Quad(_)) { "text" } else { "shape" }.into(),
            fill,
            stroke,
            norm: geom.norm(p.bbox().unwrap_or([px, py, px, py])),
        }));
    }
    Ok(None)
}

/// Which objects a recolour applies to.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Just the picked object.
    Object,
    /// Everything filled with the picked object's fill colour.
    Fill,
    /// Everything stroked with the picked object's stroke colour.
    Stroke,
    /// Everything with both the same fill and the same stroke.
    Both,
}

struct Criteria {
    mode: Mode,
    fill: Option<String>,
    stroke: Option<String>,
}

fn criteria(doc: &Document, target: PaintRef, mode: Mode) -> Result<Criteria, String> {
    let page_id = pagebox::page_id_for(doc, target.page)?;
    let walk = walk_page(doc, page_id, false);
    let p = walk.paints.get(target.index).ok_or("The selected object is no longer on the page. Pick it again.")?;
    let fill = if p.fills { colour_key(&p.fill) } else { None };
    let stroke = if p.strokes { colour_key(&p.stroke) } else { None };
    match mode {
        Mode::Fill if fill.is_none() => return Err("The selected object has no fill colour to match.".into()),
        Mode::Stroke if stroke.is_none() => return Err("The selected object has no stroke colour to match.".into()),
        Mode::Both if fill.is_none() || stroke.is_none() => {
            return Err("The selected object needs both a fill and a stroke to match both.".into())
        }
        _ => {}
    }
    Ok(Criteria { mode, fill, stroke })
}

impl Criteria {
    fn matches(&self, p: &Paint) -> bool {
        let fill_ok = || p.fills && colour_key(&p.fill).as_ref() == self.fill.as_ref();
        let stroke_ok = || p.strokes && colour_key(&p.stroke).as_ref() == self.stroke.as_ref();
        match self.mode {
            Mode::Object => false,
            Mode::Fill => fill_ok(),
            Mode::Stroke => stroke_ok(),
            Mode::Both => fill_ok() && stroke_ok(),
        }
    }

    fn change_fill(&self) -> bool {
        matches!(self.mode, Mode::Object | Mode::Fill | Mode::Both)
    }

    fn change_stroke(&self) -> bool {
        matches!(self.mode, Mode::Object | Mode::Stroke | Mode::Both)
    }
}

/// The pages to search: just `page`, or all of them.
fn pages_in_scope(doc: &Document, page: u32, all_pages: bool) -> Vec<(u32, ObjectId)> {
    doc.get_pages().into_iter().filter(|(n, _)| all_pages || *n == page).collect()
}

/// For each page: the matching paints.
fn matching(doc: &Document, target: PaintRef, crit: &Criteria, page: u32, all_pages: bool, geometry_page: u32) -> Vec<(u32, PageWalk, Vec<usize>)> {
    let mut out = Vec::new();
    let pages = if crit.mode == Mode::Object {
        pages_in_scope(doc, target.page, false)
    } else {
        pages_in_scope(doc, page, all_pages)
    };
    for (num, page_id) in pages {
        let walk = walk_page(doc, page_id, num == geometry_page);
        let hits: Vec<usize> = if crit.mode == Mode::Object {
            if num == target.page { vec![target.index].into_iter().filter(|i| *i < walk.paints.len()).collect() } else { vec![] }
        } else {
            walk.paints.iter().enumerate().filter(|(_, p)| crit.matches(p)).map(|(i, _)| i).collect()
        };
        out.push((num, walk, hits));
    }
    out
}

#[derive(Serialize)]
pub struct Found {
    /// Number of objects that would change.
    pub count: usize,
    /// Pages they're on.
    pub pages: Vec<u32>,
    /// Outlines of the ones on the current page (displayed-page fractions).
    pub norms: Vec<[f64; 4]>,
}

/// Lists what a recolour would change, with outlines for the current page.
pub fn find(path: &Path, page: u32, target: PaintRef, mode: Mode, all_pages: bool) -> Result<Found, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let crit = criteria(&doc, target, mode)?;
    let geom = page_geom(&doc, pagebox::page_id_for(&doc, page)?);
    let mut found = Found { count: 0, pages: Vec::new(), norms: Vec::new() };
    for (num, walk, hits) in matching(&doc, target, &crit, page, all_pages, page) {
        if hits.is_empty() {
            continue;
        }
        found.count += hits.len();
        found.pages.push(num);
        if num == page {
            for i in hits.iter().take(5000) {
                if let Some(b) = walk.paints[*i].bbox() {
                    found.norms.push(geom.norm(b));
                }
            }
        }
    }
    Ok(found)
}

fn cmyk_op(v: [f64; 4], stroke: bool) -> String {
    let f = |x: f64| {
        let s = format!("{:.4}", (x / 100.0).clamp(0.0, 1.0));
        let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        if s.is_empty() { "0".into() } else { s }
    };
    format!("{} {} {} {} {}", f(v[0]), f(v[1]), f(v[2]), f(v[3]), if stroke { "K" } else { "k" })
}

/// Repaints the chosen objects in CMYK (0–100 values). Returns how many
/// objects changed.
pub fn apply_recolour(
    path: &Path,
    page: u32,
    target: PaintRef,
    mode: Mode,
    all_pages: bool,
    new_fill: Option<[f64; 4]>,
    new_stroke: Option<[f64; 4]>,
) -> Result<usize, String> {
    let mut doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let crit = criteria(&doc, target, mode)?;
    let new_fill = new_fill.filter(|_| crit.change_fill());
    let new_stroke = new_stroke.filter(|_| crit.change_stroke());
    if new_fill.is_none() && new_stroke.is_none() {
        return Err("Choose a new fill or stroke colour first.".into());
    }

    // stream -> (offset -> bytes to insert), collected across pages so a
    // form shared by several pages is edited once.
    // Keyed (offset, order): an object's closing bytes (0) go before the
    // next object's opening bytes (1) if they ever land on the same offset.
    let mut edits: BTreeMap<StreamId, BTreeMap<(usize, u8), Vec<u8>>> = BTreeMap::new();
    let mut data: HashMap<StreamId, Vec<u8>> = HashMap::new();
    let mut seen: BTreeSet<(StreamId, usize, usize)> = BTreeSet::new();
    let mut count = 0;
    for (_, walk, hits) in matching(&doc, target, &crit, page, all_pages, 0) {
        for i in hits {
            let p = &walk.paints[i];
            let sid = walk.streams[p.stream];
            if !seen.insert((sid, p.start, p.end)) {
                continue; // a form drawn more than once
            }
            let fill = new_fill.filter(|_| p.fills);
            let stroke = new_stroke.filter(|_| p.strokes);
            if fill.is_none() && stroke.is_none() {
                continue;
            }
            data.entry(sid).or_insert_with(|| walk.data[p.stream].clone());
            let mut pre = String::new();
            let mut post = String::new();
            if p.wrap {
                pre.push_str("q ");
            }
            if let Some(c) = fill {
                pre.push_str(&cmyk_op(c, false));
                pre.push(' ');
            }
            if let Some(c) = stroke {
                pre.push_str(&cmyk_op(c, true));
                pre.push(' ');
            }
            let mut post_bytes = Vec::new();
            if p.wrap {
                post.push_str(" Q");
                post_bytes.extend_from_slice(post.as_bytes());
            } else {
                if fill.is_some() {
                    post_bytes.push(b' ');
                    post_bytes.extend_from_slice(&p.fill_restore);
                }
                if stroke.is_some() {
                    post_bytes.push(b' ');
                    post_bytes.extend_from_slice(&p.stroke_restore);
                }
            }
            let stream_edits = edits.entry(sid).or_default();
            stream_edits.entry((p.start, 1)).or_default().extend(pre.bytes());
            post_bytes.push(b'\n');
            stream_edits.entry((p.end, 0)).or_default().extend(post_bytes);
            count += 1;
        }
    }
    if count == 0 {
        return Ok(0);
    }

    for (sid, inserts) in edits {
        let old = data.remove(&sid).unwrap_or_default();
        let mut out = Vec::with_capacity(old.len() + inserts.values().map(|v| v.len()).sum::<usize>());
        let mut pos = 0;
        for ((at, _), bytes) in inserts {
            let at = at.min(old.len());
            out.extend_from_slice(&old[pos..at]);
            out.extend_from_slice(&bytes);
            pos = at;
        }
        out.extend_from_slice(&old[pos..]);
        match sid {
            StreamId::Page(page_id) => {
                // A new stream, in case the old one is shared with another page.
                let mut s = Stream::new(Dictionary::new(), out);
                let _ = s.compress();
                let id = doc.add_object(s);
                doc.get_dictionary_mut(page_id)
                    .map_err(|e| format!("Could not update the page: {e}"))?
                    .set("Contents", Object::Reference(id));
            }
            StreamId::Form(oid) => {
                let s = doc
                    .get_object_mut(oid)
                    .and_then(|o| o.as_stream_mut())
                    .map_err(|e| format!("Could not update a form: {e}"))?;
                s.dict.remove(b"DecodeParms");
                s.set_plain_content(out);
                let _ = s.compress();
            }
        }
    }
    doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_with_offsets() {
        let d = b"q 1 0 0 1 10 20 cm /CS0 cs 0.5 scn (a\\)b) Tj [(x) -20 (y)] TJ BI /W 1 ID \x00\xff EI Q";
        let ops = lex(d);
        let names: Vec<&str> = ops.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["q", "cm", "cs", "scn", "Tj", "TJ", "BI", "Q"]);
        assert_eq!(&d[ops[1].start..ops[1].end], b"1 0 0 1 10 20 cm");
        assert_eq!(&d[ops[4].start..ops[4].end], b"(a\\)b) Tj");
        assert!(matches!(&ops[4].args[0], Tok::Str(s) if s == b"a)b"));
    }

    #[test]
    fn cmyk_operator() {
        assert_eq!(cmyk_op([0.0, 100.0, 50.0, 12.5], false), "0 1 0.5 0.125 k");
        assert_eq!(cmyk_op([100.0, 0.0, 0.0, 0.0], true), "1 0 0 0 K");
    }

    /// `MP_RECOLOUR_PDF=path cargo test recolour_file -- --nocapture`
    #[test]
    fn recolour_file() {
        let Ok(p) = std::env::var("MP_RECOLOUR_PDF") else { return };
        let src = Path::new(&p);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.pdf");
        std::fs::copy(src, &path).unwrap();
        // Red box at top left of page 1 (x 50..200, y 650..750 of 595x842).
        let x = 100.0 / 595.0;
        let y = 1.0 - 700.0 / 842.0;
        let picked = pick(&path, 1, x, y, 2.0).unwrap().expect("hit");
        println!("picked {:?} fill {:?} stroke {:?}", picked.target, picked.fill.as_ref().map(|c| &c.label), picked.stroke.as_ref().map(|c| &c.label));
        for mode in [Mode::Object, Mode::Fill, Mode::Stroke, Mode::Both] {
            let f = find(&path, 1, picked.target, mode, true);
            println!("{mode:?}: {:?}", f.map(|f| (f.count, f.pages, f.norms.len())));
        }
        let n = apply_recolour(&path, 1, picked.target, Mode::Fill, true, Some([100.0, 0.0, 0.0, 0.0]), None).unwrap();
        println!("recoloured {n}");
        let again = pick(&path, 1, x, y, 2.0).unwrap().unwrap();
        println!("after: fill {:?}", again.fill.map(|c| c.label));
        let out = std::env::var("MP_RECOLOUR_OUT").ok();
        if let Some(o) = out {
            std::fs::copy(&path, o).unwrap();
        }
    }
}
