//! Select tool: the objects on a page (shapes, text runs, images and placed
//! graphics), which can be moved, deleted or set to overprint.
//!
//! An object is one item in the page's own content stream, so a placed
//! graphic (a form XObject, e.g. a logo placed in InDesign) is one object
//! however much it draws. Edits are spliced into the page stream as bytes,
//! like the recolour tool's: a move wraps the object as `q <shift> cm … Q`
//! (text, where `q` isn't allowed, gets a new text position and the old one
//! put back afterwards), a delete takes it out, and overprint sets an
//! ExtGState before it and restores the previous settings after it.

use crate::imageres::Matrix;
use crate::pagebox;
use crate::recolour::{page_geom, walk_page, Kind, PageGeom, PageWalk, Paint, StreamId, TopObj};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// One selectable object: the top-level item and the paints it draws.
struct Obj {
    top: TopObj,
    paints: Vec<usize>,
    /// For a clipping group: the clip's outline in page space.
    clip: Option<[f64; 4]>,
}

/// The page's objects in drawing order. Anything inside a clipping group
/// (a logo or image in its frame) belongs to the innermost such group, so
/// the frame moves with it; clips covering most of the page (artboard or
/// page clips) don't count.
fn objects(walk: &PageWalk, media: [f64; 4]) -> Vec<Obj> {
    let page_area = ((media[2] - media[0]) * (media[3] - media[1])).abs();
    let groups: Vec<_> = walk
        .groups
        .iter()
        .filter(|g| ((g.clip[2] - g.clip[0]).max(0.0) * (g.clip[3] - g.clip[1]).max(0.0)) < 0.9 * page_area)
        .collect();
    let mut out: Vec<Obj> = Vec::new();
    let mut index: HashMap<(usize, usize), usize> = HashMap::new();
    for (i, p) in walk.paints.iter().enumerate() {
        let mut top = p.top;
        let mut clip = None;
        if let Some(g) = groups.iter().filter(|g| g.start <= top.start && top.end <= g.end).min_by_key(|g| g.end - g.start) {
            top = TopObj { start: g.start, end: g.end, kind: Kind::Group, ctm: g.ctm, ovp: g.ovp };
            clip = Some(g.clip);
        }
        let key = (top.start, top.end);
        match index.get(&key) {
            Some(&k) => out[k].paints.push(i),
            None => {
                index.insert(key, out.len());
                out.push(Obj { top, paints: vec![i], clip });
            }
        }
    }
    out
}

#[derive(Serialize, Clone, Debug)]
pub struct ObjInfo {
    /// Position in the page's object order (stable across moves and
    /// overprint changes; deleting shifts the later ones).
    pub index: usize,
    pub kind: Kind,
    /// Outline as [left, top, width, height] of the displayed page.
    pub norm: [f64; 4],
    pub overprint: bool,
    /// False for a shape that also sets a clipping path.
    pub movable: bool,
    /// The object's colour as CMYK 0–100 (its fill, or its stroke if it
    /// has no fill), for "convert to spot colour".
    pub cmyk: Option<[f64; 4]>,
    /// Every colour it paints is 100% black or a spot colour: the only
    /// colours overprint is offered for.
    pub overprintable: bool,
}

/// 100% black (with or without other inks) or a spot colour.
fn overprintable_colour(c: &crate::overprint::Colour) -> bool {
    use crate::overprint::Colour;
    match c {
        Colour::Cmyk(v) => v[3] >= 0.995,
        Colour::Gray(g) => *g <= 0.005,
        Colour::Named(inks) => inks.iter().any(|(n, t)| {
            !["Cyan", "Magenta", "Yellow", "Black"].contains(&n.as_str()) || (n == "Black" && *t >= 0.995)
        }),
        _ => false,
    }
}

impl Obj {
    fn first<'a>(&self, walk: &'a PageWalk) -> &'a Paint {
        &walk.paints[self.paints[0]]
    }

    fn bbox(&self, walk: &PageWalk) -> Option<[f64; 4]> {
        let mut acc: Option<[f64; 4]> = None;
        for &i in &self.paints {
            let Some(b) = walk.paints[i].bbox() else { continue };
            acc = Some(match acc {
                None => b,
                Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
            });
        }
        // Only what shows through the clip.
        match (acc, self.clip) {
            (Some(a), Some(c)) => {
                let b = [a[0].max(c[0]), a[1].max(c[1]), a[2].min(c[2]), a[3].min(c[3])];
                Some(if b[0] < b[2] && b[1] < b[3] { b } else { c })
            }
            _ => acc,
        }
    }

    fn overprint(&self, walk: &PageWalk) -> bool {
        let o = self.top.ovp;
        match self.top.kind {
            Kind::Path | Kind::Text => {
                let p = self.first(walk);
                (p.fills && o.fill) || (p.strokes && o.stroke)
            }
            Kind::Image => o.fill,
            Kind::Form | Kind::Group => o.fill || o.stroke,
        }
    }

    fn movable(&self, walk: &PageWalk) -> bool {
        self.top.kind != Kind::Path || self.first(walk).wrap
    }

    fn info(&self, index: usize, walk: &PageWalk, geom: &PageGeom) -> Option<ObjInfo> {
        Some(ObjInfo {
            index,
            kind: self.top.kind,
            norm: geom.norm(self.bbox(walk)?),
            overprint: self.overprint(walk),
            movable: self.movable(walk),
            overprintable: self.paints.iter().all(|&i| {
                let p = &walk.paints[i];
                (!p.fills || overprintable_colour(&p.fill)) && (!p.strokes || overprintable_colour(&p.stroke))
            }),
            cmyk: self.paints.iter().find_map(|&i| {
                let p = &walk.paints[i];
                let c = if p.fills { &p.fill } else if p.strokes { &p.stroke } else { return None };
                crate::recolour::colour_info(c).map(|c| c.cmyk)
            }),
        })
    }
}

struct Loaded {
    doc: Document,
    page_id: ObjectId,
    geom: PageGeom,
    walk: PageWalk,
    objs: Vec<Obj>,
}

fn load(path: &Path, page: u32) -> Result<Loaded, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let page_id = pagebox::page_id_for(&doc, page)?;
    let geom = page_geom(&doc, page_id);
    let walk = walk_page(&doc, page_id, true);
    let objs = objects(&walk, geom.media);
    Ok(Loaded { doc, page_id, geom, walk, objs })
}

impl Loaded {
    fn infos(&self, indices: &[usize]) -> Vec<ObjInfo> {
        indices
            .iter()
            .filter_map(|&i| self.objs.get(i).and_then(|o| o.info(i, &self.walk, &self.geom)))
            .collect()
    }

    fn selected(&self, indices: &[usize]) -> Result<Vec<&Obj>, String> {
        let set: BTreeSet<usize> = indices.iter().copied().collect();
        let objs: Vec<&Obj> = set.iter().filter_map(|&i| self.objs.get(i)).collect();
        if objs.len() != set.len() {
            return Err("The selection is out of date. Select the objects again.".into());
        }
        Ok(objs)
    }

    /// Splices `edits` into the page stream and saves.
    fn save(self, path: &Path, edits: Edits) -> Result<(), String> {
        self.save_all(path, BTreeMap::from([(0, edits)]))
    }

    /// Splices edits into the page stream (stream 0) and any form streams
    /// (by their index in the walk), then saves.
    fn save_all(mut self, path: &Path, streams: StreamEdits) -> Result<(), String> {
        for (sidx, edits) in streams {
            if edits.is_empty() {
                continue;
            }
            let out = splice(&self.walk.data[sidx], edits);
            match self.walk.streams[sidx] {
                StreamId::Page(_) => replace_page_content(&mut self.doc, self.page_id, out)?,
                StreamId::Form(oid) => {
                    let s = self
                        .doc
                        .get_object_mut(oid)
                        .and_then(|o| o.as_stream_mut())
                        .map_err(|e| format!("Could not update a graphic: {e}"))?;
                    s.dict.remove(b"DecodeParms");
                    s.set_plain_content(out);
                    let _ = s.compress();
                }
            }
        }
        self.doc.save(path).map_err(|e| format!("Could not save the PDF: {e}"))?;
        Ok(())
    }

    /// A form's stream can be edited only when nothing else draws it, so
    /// the change doesn't spread to other copies.
    fn editable(&self, sidx: usize) -> bool {
        match self.walk.streams[sidx] {
            StreamId::Page(_) => true,
            StreamId::Form(oid) => references(&self.doc, oid) == 1,
        }
    }
}

fn splice(old: &[u8], edits: Edits) -> Vec<u8> {
    let mut out = Vec::with_capacity(old.len() + 64 * edits.len());
    let mut pos = 0;
    for ((at, _), (end, bytes)) in edits {
        let at = at.min(old.len()).max(pos);
        out.extend_from_slice(&old[pos..at]);
        out.extend_from_slice(&bytes);
        pos = end.min(old.len()).max(at);
    }
    out.extend_from_slice(&old[pos..]);
    out
}

/// How many times the document refers to object `id`.
fn references(doc: &Document, id: ObjectId) -> usize {
    fn count(o: &Object, id: ObjectId) -> usize {
        match o {
            Object::Reference(r) => (*r == id) as usize,
            Object::Array(a) => a.iter().map(|x| count(x, id)).sum(),
            Object::Dictionary(d) => d.iter().map(|(_, x)| count(x, id)).sum(),
            Object::Stream(s) => s.dict.iter().map(|(_, x)| count(x, id)).sum(),
            _ => 0,
        }
    }
    doc.objects.values().map(|o| count(o, id)).sum()
}

/// Removes a transparency group from a form that has nothing transparent
/// in it. An isolated group is drawn on its own and then pasted over the
/// page, so its contents can never overprint what's underneath; without
/// any transparency inside, the group makes no other difference.
fn drop_needless_group(doc: &mut Document, oid: ObjectId) {
    let Ok(Object::Stream(s)) = doc.get_object(oid) else { return };
    if !s.dict.has(b"Group") {
        return;
    }
    let res = s.dict.get(b"Resources").ok().and_then(|r| doc.dereference(r).ok()).and_then(|(_, o)| o.as_dict().ok());
    let transparent = res.is_some_and(|res| {
        let gs_alpha = res
            .get(b"ExtGState")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_dict().ok())
            .is_some_and(|gss| {
                gss.iter().any(|(_, g)| {
                    let Some(g) = doc.dereference(g).ok().and_then(|(_, o)| o.as_dict().ok()) else { return false };
                    let alpha = |k: &[u8]| g.get(k).ok().and_then(|v| v.as_float().ok().or(v.as_i64().ok().map(|i| i as f32))).is_some_and(|a| a < 0.999);
                    let smask = g.get(b"SMask").ok().is_some_and(|m| m.as_name().map_or(true, |n| n != b"None"));
                    let blend = g.get(b"BM").ok().is_some_and(|b| b.as_name().map_or(true, |n| n != b"Normal" && n != b"Compatible"));
                    alpha(b"CA") || alpha(b"ca") || smask || blend
                })
            });
        let masked_image = res
            .get(b"XObject")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_dict().ok())
            .is_some_and(|xs| {
                xs.iter().any(|(_, x)| matches!(doc.dereference(x).map(|(_, o)| o), Ok(Object::Stream(st)) if st.dict.has(b"SMask") || st.dict.has(b"Group")))
            });
        gs_alpha || masked_image
    });
    if !transparent {
        if let Ok(Object::Stream(s)) = doc.get_object_mut(oid) {
            s.dict.remove(b"Group");
        }
    }
}

/// Adds a resource to a form's own resources (creating them if needed).
fn add_form_resource(doc: &mut Document, oid: ObjectId, category: &[u8], name: &[u8], value: Object) -> Result<(), String> {
    let err = |e: lopdf::Error| format!("Could not update a graphic's resources: {e}");
    let own = doc.get_object(oid).and_then(|o| o.as_stream()).map_err(err)?.dict.get(b"Resources").ok().cloned();
    let res: &mut Dictionary = match own {
        Some(Object::Reference(id)) => doc.get_dictionary_mut(id).map_err(err)?,
        _ => {
            let s = doc.get_object_mut(oid).and_then(|o| o.as_stream_mut()).map_err(err)?;
            if !matches!(s.dict.get(b"Resources"), Ok(Object::Dictionary(_))) {
                s.dict.set("Resources", Object::Dictionary(Dictionary::new()));
            }
            s.dict.get_mut(b"Resources").and_then(Object::as_dict_mut).map_err(err)?
        }
    };
    match res.get(category).ok().cloned() {
        Some(Object::Reference(cid)) => {
            doc.get_dictionary_mut(cid).map_err(err)?.set(name.to_vec(), value);
        }
        Some(Object::Dictionary(_)) => {
            res.get_mut(category).and_then(Object::as_dict_mut).map_err(err)?.set(name.to_vec(), value);
        }
        _ => {
            let mut d = Dictionary::new();
            d.set(name.to_vec(), value);
            res.set(category.to_vec(), Object::Dictionary(d));
        }
    }
    Ok(())
}

/// Page stream edits: (offset, order) → (end of the replaced bytes, new
/// bytes). An insert replaces nothing (end = offset). At the same offset,
/// one object's closing bytes (order 0) go before the next one's opening
/// bytes (order 1).
type Edits = BTreeMap<(usize, u8), (usize, Vec<u8>)>;

/// Edits for several streams: the page's (0) and forms', by walk index.
type StreamEdits = BTreeMap<usize, Edits>;

fn insert(edits: &mut Edits, at: usize, order: u8, bytes: String) {
    edits.entry((at, order)).or_insert((at, Vec::new())).1.extend(bytes.into_bytes());
}

/// Gives the page a single new content stream (the old one may be shared
/// with other pages).
pub(crate) fn replace_page_content(doc: &mut Document, page_id: ObjectId, data: Vec<u8>) -> Result<(), String> {
    let mut s = Stream::new(Dictionary::new(), data);
    let _ = s.compress();
    let id = doc.add_object(s);
    doc.get_dictionary_mut(page_id)
        .map_err(|e| format!("Could not update the page: {e}"))?
        .set("Contents", Object::Reference(id));
    Ok(())
}

/// A number for a content stream: up to 4 decimals, no trailing zeros.
fn num(v: f64) -> String {
    let s = format!("{:.4}", if v.abs() < 1e-9 { 0.0 } else { v });
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() { "0".into() } else { s.to_string() }
}

fn matrix(m: &Matrix) -> String {
    m.iter().map(|v| num(*v)).collect::<Vec<_>>().join(" ")
}

/// The shift in an object's own space that moves it by (`dx`, `dy`) on the
/// page, given the transformation `m` in force there.
fn local_shift(m: &Matrix, dx: f64, dy: f64) -> Option<(f64, f64)> {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-12 {
        return None;
    }
    Some(((m[3] * dx - m[2] * dy) / det, (-m[1] * dx + m[0] * dy) / det))
}

/// `TJ` that moves the text position `adv` along the line without drawing.
fn advance(adv: f64, font_size: f64, h_scale: f64) -> String {
    let k = font_size * h_scale;
    if adv.abs() < 1e-9 || k.abs() < 1e-9 {
        return String::new();
    }
    format!("[{}] TJ", num(-adv * 1000.0 / k))
}

// ---------------------------------------------------------------------------
// Commands

/// The topmost object at a point (`x`, `y` fractions of the displayed page,
/// `tol` hit tolerance in points).
pub fn pick(path: &Path, page: u32, x: f64, y: f64, tol: f64) -> Result<Option<ObjInfo>, String> {
    let l = load(path, page)?;
    let (px, py) = l.geom.to_pdf(x, y);
    for (pi, p) in l.walk.paints.iter().enumerate().rev() {
        if !p.hit(px, py, tol) {
            continue;
        }
        let Some(i) = l.objs.iter().position(|o| o.paints.contains(&pi)) else { continue };
        if let Some(c) = l.objs[i].clip {
            if px < c[0] - tol || px > c[2] + tol || py < c[1] - tol || py > c[3] + tol {
                continue; // that part is clipped away
            }
        }
        return Ok(l.objs[i].info(i, &l.walk, &l.geom));
    }
    Ok(None)
}

/// Objects lying wholly inside `rect` ([left, top, width, height] of the
/// displayed page).
pub fn in_rect(path: &Path, page: u32, rect: [f64; 4]) -> Result<Vec<ObjInfo>, String> {
    let l = load(path, page)?;
    let [rl, rt, rw, rh] = rect;
    let eps = 1e-6;
    Ok((0..l.objs.len())
        .filter_map(|i| l.objs[i].info(i, &l.walk, &l.geom))
        .filter(|o| {
            let [x, y, w, h] = o.norm;
            x >= rl - eps && y >= rt - eps && x + w <= rl + rw + eps && y + h <= rt + rh + eps
        })
        .collect())
}

/// Current outlines and settings of the given objects.
pub fn info(path: &Path, page: u32, indices: &[usize]) -> Result<Vec<ObjInfo>, String> {
    Ok(load(path, page)?.infos(indices))
}

#[derive(Serialize)]
pub struct EditResult {
    /// Objects changed.
    pub changed: usize,
    /// Objects that couldn't be changed (e.g. shapes that also clip).
    pub skipped: usize,
    /// The selection afterwards (empty after a delete).
    pub objects: Vec<ObjInfo>,
}

/// Moves the objects by (`dx`, `dy`), fractions of the displayed page.
pub fn move_objects(path: &Path, page: u32, indices: &[usize], dx: f64, dy: f64) -> Result<EditResult, String> {
    let l = load(path, page)?;
    let (x0, y0) = l.geom.to_pdf(0.0, 0.0);
    let (x1, y1) = l.geom.to_pdf(dx, dy);
    let (pdx, pdy) = (x1 - x0, y1 - y0);
    let mut edits = Edits::new();
    let (mut changed, mut skipped) = (0, 0);
    for o in l.selected(indices)? {
        let Some((tx, ty)) = local_shift(&o.top.ctm, pdx, pdy) else {
            skipped += 1;
            continue;
        };
        let p = o.first(&l.walk);
        match (o.top.kind, &p.text) {
            (Kind::Text, Some(t)) => {
                let mut m = t.tm;
                m[4] += tx;
                m[5] += ty;
                // `'` and `"` move down a line first: start a line higher.
                if t.op == "'" || t.op == "\"" {
                    m[4] += t.leading * m[2];
                    m[5] += t.leading * m[3];
                }
                insert(&mut edits, o.top.start, 1, format!("{} Tm ", matrix(&m)));
                let adv = advance(t.u_after, t.font_size, t.h_scale);
                insert(&mut edits, o.top.end, 0, format!(" {} Tm {adv}\n", matrix(&t.tlm)));
            }
            _ if o.movable(&l.walk) => {
                insert(&mut edits, o.top.start, 1, format!("q 1 0 0 1 {} {} cm ", num(tx), num(ty)));
                insert(&mut edits, o.top.end, 0, " Q\n".into());
            }
            _ => {
                skipped += 1;
                continue;
            }
        }
        changed += 1;
    }
    if changed > 0 {
        l.save(path, edits)?;
    }
    Ok(EditResult { changed, skipped, objects: info(path, page, indices)? })
}

/// Removes the objects from the page.
pub fn delete_objects(path: &Path, page: u32, indices: &[usize]) -> Result<EditResult, String> {
    let l = load(path, page)?;
    let mut edits = Edits::new();
    let mut changed = 0;
    for o in l.selected(indices)? {
        let p = o.first(&l.walk);
        let (start, end, with) = match (o.top.kind, &p.text) {
            (Kind::Text, Some(t)) => {
                // Keep the text position moving on as before, so the rest
                // of the line stays where it was.
                let line = match t.op.as_str() {
                    "'" => "T* ".to_string(),
                    "\"" if t.nums.len() >= 2 => format!("{} Tw {} Tc T* ", num(t.nums[0]), num(t.nums[1])),
                    _ => String::new(),
                };
                (o.top.start, o.top.end, format!(" {line}{} ", advance(t.adv, t.font_size, t.h_scale)))
            }
            // A shape that also clips keeps its clip: just stop painting it.
            (Kind::Path, _) if !p.wrap => (p.paint_op.0, p.paint_op.1, " n ".to_string()),
            _ => (o.top.start, o.top.end, " ".to_string()),
        };
        edits.insert((start, 1), (end, with.into_bytes()));
        changed += 1;
    }
    if changed > 0 {
        l.save(path, edits)?;
    }
    Ok(EditResult { changed, skipped: 0, objects: Vec::new() })
}

/// Overprint only keeps the inks underneath when the colour is plain
/// DeviceCMYK: grey, or CMYK in a colour profile, is painted as a full
/// colour and knocks out. So an object being set to overprint has such a
/// colour re-set as the same CMYK values for the duration (grey g becomes
/// K = 1 - g). Returns (bytes before, bytes after) for a fill or stroke.
fn as_device_cmyk(colour: &crate::overprint::Colour, restore: &[u8], stroke: bool) -> Option<(String, String)> {
    use crate::overprint::Colour;
    let op = if stroke { "K" } else { "k" };
    let already = String::from_utf8_lossy(restore).trim_end().ends_with(&format!(" {op}"));
    if already {
        return None;
    }
    let v = match colour {
        Colour::Gray(g) => [0.0, 0.0, 0.0, 1.0 - g],
        Colour::Cmyk(c) => *c,
        _ => return None, // RGB and spot colours have no plain CMYK value
    };
    let set = format!("{} {} {} {} {op}", num(v[0]), num(v[1]), num(v[2]), num(v[3]));
    Some((set, String::from_utf8_lossy(restore).into_owned()))
}

/// Repaints the objects in a new spot colour `name` whose CMYK equivalent
/// (0–100) is `cmyk`: each shape or text run's fill becomes the spot at
/// 100% (or its stroke, when it has no fill). Placed graphics can't be
/// changed (that would change every copy) and count as skipped.
pub fn to_spot(path: &Path, page: u32, indices: &[usize], name: &str, cmyk: [f64; 4]) -> Result<EditResult, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the spot colour a name.".into());
    }
    let mut l = load(path, page)?;
    let mut edits = StreamEdits::new();
    let (mut changed, mut skipped) = (0, 0);
    // A resource name of our own for this spot.
    let key = format!("MPspot{}", name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)));
    let mut forms: BTreeSet<ObjectId> = BTreeSet::new();
    for o in l.selected(indices)? {
        let mut any = false;
        for &i in &o.paints {
            let p = &l.walk.paints[i];
            if (!p.fills && !p.strokes) || !l.editable(p.stream) {
                continue;
            }
            let (set, restore) = if p.fills {
                (format!("/{key} cs 1 scn "), p.fill_restore.clone())
            } else {
                (format!("/{key} CS 1 SCN "), p.stroke_restore.clone())
            };
            let e = edits.entry(p.stream).or_default();
            insert(e, p.start, 1, set);
            insert(e, p.end, 0, format!(" {}\n", String::from_utf8_lossy(&restore)));
            if let StreamId::Form(oid) = l.walk.streams[p.stream] {
                forms.insert(oid);
            }
            any = true;
        }
        if any { changed += 1 } else { skipped += 1 }
    }
    if changed > 0 {
        let c = cmyk.map(|v| Object::Real((v / 100.0).clamp(0.0, 1.0) as f32));
        let mut tint = Dictionary::new();
        tint.set("FunctionType", Object::Integer(2));
        tint.set("Domain", Object::Array(vec![0.into(), 1.into()]));
        tint.set("C0", Object::Array(vec![Object::Real(0.0); 4]));
        tint.set("C1", Object::Array(c.to_vec()));
        tint.set("N", Object::Integer(1));
        let space = Object::Array(vec![
            Object::Name(b"Separation".to_vec()),
            Object::Name(name.as_bytes().to_vec()),
            Object::Name(b"DeviceCMYK".to_vec()),
            Object::Dictionary(tint),
        ]);
        let page_id = l.page_id;
        add_resource(&mut l.doc, page_id, b"ColorSpace", key.as_bytes(), space.clone())?;
        for oid in forms {
            add_form_resource(&mut l.doc, oid, b"ColorSpace", key.as_bytes(), space.clone())?;
        }
        l.save_all(path, edits)?;
    }
    Ok(EditResult { changed, skipped, objects: info(path, page, indices)? })
}

/// ExtGState resource name for a set of overprint settings.
fn ovp_name(fill: bool, stroke: bool, mode: i64) -> String {
    format!("MPop{}{}{}", fill as u8, stroke as u8, mode.clamp(0, 1))
}

/// Turns overprint on (fill and stroke, overprint mode 1) or off.
pub fn set_overprint(path: &Path, page: u32, indices: &[usize], on: bool) -> Result<EditResult, String> {
    let mut l = load(path, page)?;
    let mut edits = StreamEdits::new();
    let mut names: BTreeSet<(bool, bool, i64)> = BTreeSet::new();
    let mut changed = 0;
    let mut groups: BTreeSet<ObjectId> = BTreeSet::new();
    let mut multiply = false;
    // Objects that may not overprint (not 100% black or a spot colour).
    let mut rgb = 0;
    for o in l.selected(indices)? {
        let was = o.top.ovp;
        // Only 100% black and spot colours may overprint.
        if on && !o.info(0, &l.walk, &l.geom).is_some_and(|i| i.overprintable) {
            rgb += 1;
            continue;
        }
        let want = if on { (true, true, 1) } else { (false, false, was.mode) };
        if (was.fill, was.stroke, was.mode) == want {
            continue;
        }
        let back = (was.fill, was.stroke, was.mode.clamp(0, 1));
        names.insert(want);
        names.insert(back);
        // An image can't overprint (overprint only applies to flat
        // colours), so images get the Multiply blend instead, which has
        // the same effect: the colours underneath show through.
        let has_image = o.paints.iter().any(|&i| !l.walk.paints[i].fills && !l.walk.paints[i].strokes);
        let image_mode = on && has_image && o.top.kind != Kind::Path && o.top.kind != Kind::Text;
        let (mut pre, mut post) = if image_mode {
            multiply = true;
            (format!("q /{} gs /MPmultiply gs ", ovp_name(want.0, want.1, want.2)), " Q".to_string())
        } else {
            (format!("/{} gs ", ovp_name(want.0, want.1, want.2)), format!(" /{} gs", ovp_name(back.0, back.1, back.2)))
        };
        // Forms drawn by the object: their isolated groups would stop the
        // overprint reaching the page.
        if on {
            for &i in &o.paints {
                if let StreamId::Form(oid) = l.walk.streams[l.walk.paints[i].stream] {
                    groups.insert(oid);
                }
            }
        }
        // Colour conversions: around the object itself, or around each
        // shape and text run inside a clipping group (a placed graphic's
        // own content can't be changed without changing every copy).
        let mut inner: Vec<(usize, usize, usize, String, String)> = Vec::new();
        if on
            && o.paints.iter().any(|&i| {
                let p = &l.walk.paints[i];
                (p.fills && matches!(p.fill, crate::overprint::Colour::Rgb(_)))
                    || (p.strokes && matches!(p.stroke, crate::overprint::Colour::Rgb(_)))
            })
        {
            rgb += 1;
        }
        if on {
            for &i in &o.paints {
                let p = &l.walk.paints[i];
                if (!p.fills && !p.strokes) || !l.editable(p.stream) {
                    continue;
                }
                let conv = [
                    p.fills.then(|| as_device_cmyk(&p.fill, &p.fill_restore, false)).flatten(),
                    p.strokes.then(|| as_device_cmyk(&p.stroke, &p.stroke_restore, true)).flatten(),
                ];
                let (mut set, mut restore) = (String::new(), String::new());
                for (s, r) in conv.into_iter().flatten() {
                    set.push_str(&s);
                    set.push(' ');
                    restore.push(' ');
                    restore.push_str(&r);
                }
                if !set.is_empty() {
                    inner.push((p.stream, p.start, p.end, set, restore));
                }
            }
        }
        if matches!(o.top.kind, Kind::Path | Kind::Text) {
            if let Some((_, _, _, set, restore)) = inner.into_iter().next() {
                pre.push_str(&set);
                post.push_str(&restore);
            }
        } else {
            for (sidx, start, end, set, restore) in inner {
                let e = edits.entry(sidx).or_default();
                insert(e, start, 1, set);
                insert(e, end, 0, format!("{restore}\n"));
            }
        }
        post.push('\n');
        let page_edits = edits.entry(0).or_default();
        insert(page_edits, o.top.start, 1, pre);
        insert(page_edits, o.top.end, 0, post);
        changed += 1;
    }
    if changed > 0 {
        let page_id = l.page_id;
        for (f, s, m) in names {
            let mut gs = Dictionary::new();
            gs.set("Type", Object::Name(b"ExtGState".to_vec()));
            gs.set("OP", Object::Boolean(s));
            gs.set("op", Object::Boolean(f));
            gs.set("OPM", Object::Integer(m));
            add_resource(&mut l.doc, page_id, b"ExtGState", ovp_name(f, s, m).as_bytes(), Object::Dictionary(gs))?;
        }
        if multiply {
            let mut gs = Dictionary::new();
            gs.set("Type", Object::Name(b"ExtGState".to_vec()));
            gs.set("BM", Object::Name(b"Multiply".to_vec()));
            add_resource(&mut l.doc, page_id, b"ExtGState", b"MPmultiply", Object::Dictionary(gs))?;
        }
        for oid in groups {
            drop_needless_group(&mut l.doc, oid);
        }
        l.save_all(path, edits)?;
    }
    Ok(EditResult { changed, skipped: rgb, objects: info(path, page, indices)? })
}

/// Adds `name` → `value` to one category (ExtGState, XObject, …) of the
/// page's resources, giving the page its own resources if it only
/// inherits them.
pub(crate) fn add_resource(doc: &mut Document, page_id: ObjectId, category: &[u8], name: &[u8], value: Object) -> Result<(), String> {
    let err = |e: lopdf::Error| format!("Could not update the page resources: {e}");
    // Where the page's resource dictionary lives.
    let own = doc.get_dictionary(page_id).map_err(err)?.get(b"Resources").ok().cloned();
    let res_id = match own {
        Some(Object::Reference(id)) => Some(id),
        Some(Object::Dictionary(_)) => None,
        _ => {
            // Inherited (or missing): copy the nearest ancestor's onto the page.
            let mut found = Dictionary::new();
            let mut id = page_id;
            while let Ok(Object::Reference(pid)) = doc.get_dictionary(id).map_err(err)?.get(b"Parent").cloned() {
                id = pid;
                if let Ok(r) = doc.get_dictionary(id).map_err(err)?.get(b"Resources") {
                    if let Ok(d) = doc.dereference(r).and_then(|(_, o)| o.as_dict()) {
                        found = d.clone();
                    }
                    break;
                }
            }
            doc.get_dictionary_mut(page_id).map_err(err)?.set("Resources", Object::Dictionary(found));
            None
        }
    };
    let res: &mut Dictionary = match res_id {
        Some(id) => doc.get_dictionary_mut(id).map_err(err)?,
        None => doc.get_dictionary_mut(page_id).map_err(err)?.get_mut(b"Resources").and_then(Object::as_dict_mut).map_err(err)?,
    };
    match res.get(category).ok().cloned() {
        Some(Object::Reference(cid)) => {
            doc.get_dictionary_mut(cid).map_err(err)?.set(name.to_vec(), value);
        }
        Some(Object::Dictionary(_)) => {
            res.get_mut(category).and_then(Object::as_dict_mut).map_err(err)?.set(name.to_vec(), value);
        }
        _ => {
            let mut d = Dictionary::new();
            d.set(name.to_vec(), value);
            res.set(category.to_vec(), Object::Dictionary(d));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(-0.00001), "0");
        assert_eq!(num(12.34567), "12.3457");
        assert_eq!(num(-3.5), "-3.5");
    }

    /// `MP_OBJECTS_PDF=file.pdf MP_OBJECTS_OUT=out.pdf cargo test objects_file -- --nocapture`
    /// (page 1: a box at x 50–200, y 650–750 and text at 72,400 of 595x842).
    #[test]
    fn objects_file() {
        let Ok(src) = std::env::var("MP_OBJECTS_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let at = |x: f64, y: f64| (x / 595.0, 1.0 - y / 842.0);
        let (bx, by) = at(100.0, 700.0);
        let boxed = pick(&p, 1, bx, by, 2.0).unwrap().expect("box");
        println!("box {boxed:?}");
        let moved = move_objects(&p, 1, &[boxed.index], 0.1, 0.05).unwrap();
        let after = &moved.objects[0];
        assert!((after.norm[0] - boxed.norm[0] - 0.1).abs() < 1e-3 && (after.norm[1] - boxed.norm[1] - 0.05).abs() < 1e-3, "{after:?}");
        let op = set_overprint(&p, 1, &[boxed.index], true).unwrap();
        assert!(op.objects[0].overprint);
        let (tx, ty) = at(80.0, 410.0);
        let text = pick(&p, 1, tx, ty, 2.0).unwrap().expect("text");
        println!("text {text:?}");
        let moved = move_objects(&p, 1, &[text.index], -0.05, -0.1).unwrap();
        println!("text moved {:?}", moved.objects);
        let all = in_rect(&p, 1, [0.0, 0.0, 1.0, 1.0]).unwrap();
        println!("all {}", all.len());
        let (lx, ly) = at(150.0, 240.0);
        let purple = pick(&p, 1, lx, ly, 2.0).unwrap().expect("purple");
        let before = in_rect(&p, 1, [0.0, 0.0, 1.0, 1.0]).unwrap().len();
        delete_objects(&p, 1, &[purple.index]).unwrap();
        assert_eq!(in_rect(&p, 1, [0.0, 0.0, 1.0, 1.0]).unwrap().len(), before - 1);
        if let Ok(out) = std::env::var("MP_OBJECTS_OUT") {
            std::fs::copy(&p, out).unwrap();
        }
    }

    /// `MP_CLIP_PDF=file.pdf MP_CLIP_OUT=out.pdf cargo test clip_file -- --nocapture`
    /// (a red disc clipped to x 100–300, y 600–700 of 595x842).
    #[test]
    fn clip_file() {
        let Ok(src) = std::env::var("MP_CLIP_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let hit = pick(&p, 1, 200.0 / 595.0, 1.0 - 650.0 / 842.0, 2.0).unwrap().expect("hit");
        println!("{hit:?}");
        assert_eq!(hit.kind, Kind::Group);
        // Outside the clip the background is what's hit.
        let bg = pick(&p, 1, 200.0 / 595.0, 1.0 - 750.0 / 842.0, 2.0).unwrap().expect("bg");
        assert_ne!(bg.index, hit.index);
        let moved = move_objects(&p, 1, &[hit.index], 0.2, 0.3).unwrap();
        println!("{:?}", moved.objects);
        if let Ok(out) = std::env::var("MP_CLIP_OUT") {
            std::fs::copy(&p, out).unwrap();
        }
    }

    /// `MP_OP_PDF=file.pdf MP_OP_OUT=out.pdf cargo test overprint_file -- --nocapture`
    /// (text on a cyan background, 400x300: "CMYK" at y 180, "GRAY" at y 60).
    #[test]
    fn overprint_file() {
        let Ok(src) = std::env::var("MP_OP_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let a = pick(&p, 1, 60.0 / 400.0, 1.0 - 200.0 / 300.0, 2.0).unwrap().expect("cmyk text");
        let b = pick(&p, 1, 60.0 / 400.0, 1.0 - 80.0 / 300.0, 2.0).unwrap().expect("gray text");
        println!("{a:?}\n{b:?}");
        let r = set_overprint(&p, 1, &[a.index, b.index], true).unwrap();
        println!("{:?}", r.objects);
        let doc = Document::load(&p).unwrap();
        let id = pagebox::page_id_for(&doc, 1).unwrap();
        println!("{}", String::from_utf8_lossy(&doc.get_page_content(id)));
        // Where the text is, the cyan background should still be there.
        let plates = tempfile::TempDir::new().unwrap();
        let found = crate::gs::ensure_separations(&p, 1, 72, plates.path()).unwrap();
        let cyan = found.iter().find(|(n, _)| n == "Cyan").map(|(_, f)| image::open(f).unwrap().to_luma8()).unwrap();
        for (name, o) in [("CMYK", &a), ("GRAY", &b)] {
            let [x, y, w, h] = o.norm;
            let (iw, ih) = (cyan.width() as f64, cyan.height() as f64);
            let mut gaps = 0;
            for py in (y * ih) as u32..((y + h) * ih) as u32 {
                for px in (x * iw) as u32..((x + w) * iw) as u32 {
                    if cyan.get_pixel(px, py).0[0] > 128 {
                        gaps += 1; // little cyan ink here: knocked out
                    }
                }
            }
            println!("{name}: {gaps} knocked-out pixels");
        }
        if let Ok(out) = std::env::var("MP_OP_OUT") {
            std::fs::copy(&p, out).unwrap();
        }
    }

    /// Knocked-out pixels of the cyan plate inside `norm`.
    fn cyan_gaps(p: &Path, norm: [f64; 4]) -> usize {
        let plates = tempfile::TempDir::new().unwrap();
        let found = crate::gs::ensure_separations(p, 1, 72, plates.path()).unwrap();
        let cyan = found.iter().find(|(n, _)| n == "Cyan").map(|(_, f)| image::open(f).unwrap().to_luma8()).unwrap();
        let [x, y, w, h] = norm;
        let (iw, ih) = (cyan.width() as f64, cyan.height() as f64);
        let mut gaps = 0;
        for py in (y * ih) as u32..((y + h) * ih) as u32 {
            for px in (x * iw) as u32..((x + w) * iw) as u32 {
                if cyan.get_pixel(px.min(cyan.width() - 1), py.min(cyan.height() - 1)).0[0] > 128 {
                    gaps += 1;
                }
            }
        }
        gaps
    }

    /// `MP_OP_PDF=file.pdf cargo test merged_overprint -- --nocapture`:
    /// mail merge text, then overprint set on it from the Select tool.
    #[test]
    fn merged_overprint() {
        let Ok(src) = std::env::var("MP_OP_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let field = crate::pages::MergeField {
            rect: [0.5, 0.3, 0.45, 0.2],
            font: "sans-bold".into(),
            size: 40.0,
            cmyk: [0.0, 0.0, 0.0, 100.0],
            align: "left".into(),
            valign: "middle".into(),
            shrink: true,
            overprint: false,
        };
        crate::pages::mail_merge(&p, 1, &[vec!["Merged".into()]], &crate::pages::MergeSpec { fields: vec![field] }).unwrap();
        let hit = pick(&p, 1, 0.6, 0.4, 2.0).unwrap().expect("merged text");
        println!("{hit:?} gaps before {}", cyan_gaps(&p, hit.norm));
        set_overprint(&p, 1, &[hit.index], true).unwrap();
        println!("gaps after {}", cyan_gaps(&p, hit.norm));
        let doc = Document::load(&p).unwrap();
        let id = pagebox::page_id_for(&doc, 1).unwrap();
        println!("{}", String::from_utf8_lossy(&doc.get_page_content(id)));
    }

    /// `MP_DUMP_PDF=file.pdf cargo test dump_objects -- --nocapture`:
    /// every object on page 1 and the start of the page stream.
    #[test]
    fn dump_objects() {
        let Ok(src) = std::env::var("MP_DUMP_PDF") else { return };
        let page: u32 = std::env::var("MP_DUMP_PAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        let l = load(Path::new(&src), page).unwrap();
        println!("media {:?} rotate {}", l.geom.media, l.geom.rotate);
        println!("groups {:?}", l.walk.groups);
        for (i, o) in l.objs.iter().enumerate() {
            let info = o.info(i, &l.walk, &l.geom);
            let p = o.first(&l.walk);
            println!(
                "#{i} {:?} paints={} ovp={:?} fill={:?} stroke={:?} fills={} strokes={} {:?}",
                o.top.kind, o.paints.len(), o.top.ovp, p.fill, p.stroke, p.fills, p.strokes, info.map(|x| x.norm)
            );
        }
        // XObjects on the page, without their data.
        if let Ok((Some(res), _)) = l.doc.get_page_resources(l.page_id) {
            if let Ok(Object::Dictionary(x)) = res.get_deref(b"XObject", &l.doc) {
                for (name, r) in x.iter() {
                    if let Ok(Object::Stream(s)) = l.doc.dereference(r).map(|(_, o)| o) {
                        let mut d = s.dict.clone();
                        d.remove(b"Length");
                        println!("XObject {} = {:?}", String::from_utf8_lossy(name), d);
                        if let Ok(g) = s.dict.get(b"Group") {
                            println!("  Group {:?}", l.doc.dereference(g).map(|(_, o)| o.clone()));
                            let c = s.decompressed_content().unwrap_or_default();
                            println!("  content: {}", String::from_utf8_lossy(&c[..c.len().min(400)]));
                        }
                        if let Ok(sm) = s.dict.get(b"SMask") {
                            if let Ok(Object::Stream(m)) = l.doc.dereference(sm).map(|(_, o)| o) {
                                println!("  SMask {:?}", m.dict);
                            }
                        }
                    }
                }
            }
        }
        let data = &l.walk.data[0];
        println!("--- page stream ({} bytes) ---\n{}", data.len(), String::from_utf8_lossy(&data[..data.len().min(3000)]));
    }

    /// `MP_CARD_PDF=file.pdf MP_CARD_OUT=out.pdf cargo test card_overprint -- --nocapture`:
    /// a line of text inside an isolated transparency group on page 2,
    /// made a spot colour and overprinted, over a rich black background.
    #[test]
    fn card_overprint() {
        let Ok(src) = std::env::var("MP_CARD_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let gaps2 = |p: &Path, norm: [f64; 4]| {
            let plates = tempfile::TempDir::new().unwrap();
            let found = crate::gs::ensure_separations(p, 2, 150, plates.path()).unwrap();
            let cyan = found.iter().find(|(n, _)| n == "Cyan").map(|(_, f)| image::open(f).unwrap().to_luma8()).unwrap();
            let [x, y, w, h] = norm;
            let (iw, ih) = (cyan.width() as f64, cyan.height() as f64);
            let mut gaps = 0;
            for py in (y * ih) as u32..((y + h) * ih) as u32 {
                for px in (x * iw) as u32..((x + w) * iw) as u32 {
                    if cyan.get_pixel(px, py).0[0] > 200 {
                        gaps += 1;
                    }
                }
            }
            gaps
        };
        let hit = pick(&p, 2, 0.2, 0.8, 1.0).unwrap().expect("text line");
        println!("{hit:?} gaps before {}", gaps2(&p, hit.norm));
        let r = to_spot(&p, 2, &[hit.index], "Spot Gold", hit.cmyk.unwrap()).unwrap();
        println!("spot changed {} skipped {}", r.changed, r.skipped);
        let after = info(&p, 2, &[hit.index]).unwrap();
        println!("after spot {:?} gaps {}", after, gaps2(&p, hit.norm));
        let r = set_overprint(&p, 2, &[hit.index], true).unwrap();
        println!("overprint changed {} skipped {}", r.changed, r.skipped);
        println!("gaps after {}", gaps2(&p, hit.norm));
        // The logo on page 1 is an image with transparency.
        let logo = pick(&p, 1, 0.45, 0.5, 1.0).unwrap().expect("logo");
        println!("logo {logo:?}");
        if let Ok(out) = std::env::var("MP_CARD_OUT") {
            std::fs::copy(&p, out).unwrap();
        }
    }

    /// `MP_OBJECTS_PDF=file.pdf cargo test spot_file -- --nocapture`
    #[test]
    fn spot_file() {
        let Ok(src) = std::env::var("MP_OBJECTS_PDF") else { return };
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("t.pdf");
        std::fs::copy(&src, &p).unwrap();
        let boxed = pick(&p, 1, 100.0 / 595.0, 1.0 - 700.0 / 842.0, 2.0).unwrap().expect("box");
        let r = to_spot(&p, 1, &[boxed.index], "Spot Red", boxed.cmyk.unwrap()).unwrap();
        assert_eq!(r.changed, 1);
        let spots = crate::colorcheck::spot_names(&p).unwrap();
        println!("spots {spots:?}, colour {:?}", r.objects[0].cmyk);
        assert!(spots.iter().any(|s| s == "Spot Red"));
        let plates = tempfile::TempDir::new().unwrap();
        let found = crate::gs::ensure_separations(&p, 1, 36, plates.path()).unwrap();
        println!("plates {:?}", found.iter().map(|(n, _)| n).collect::<Vec<_>>());
        assert!(found.iter().any(|(n, _)| n == "Spot Red"));
    }

    #[test]
    fn shift_through_scale_and_rotation() {
        // 2x scale: a 10 pt page move is 5 units locally.
        assert_eq!(local_shift(&[2.0, 0.0, 0.0, 2.0, 7.0, 9.0], 10.0, 4.0), Some((5.0, 2.0)));
        // 90° rotation.
        let (x, y) = local_shift(&[0.0, 1.0, -1.0, 0.0, 0.0, 0.0], 10.0, 0.0).unwrap();
        assert!((x - 0.0).abs() < 1e-9 && (y + 10.0).abs() < 1e-9);
    }
}
