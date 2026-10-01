//! Display colours for spot inks. Each Separation colour space carries an
//! alternate space (CMYK, RGB, Lab, grey) and a tint transform function; we
//! evaluate that function at 100% tint and convert the result to sRGB for
//! the swatch next to the plate in the Separations list.

use crate::colorcheck::deref;
use lopdf::{Document, Object, Stream};
use std::collections::BTreeMap;
use std::path::Path;

/// Evaluates a PDF function (types 0, 2, 3 and 4) at the input `x`.
fn eval_function(doc: &Document, f: &Object, x: f64, depth: u32) -> Option<Vec<f64>> {
    if depth > 6 {
        return None;
    }
    let (dict, stream): (&lopdf::Dictionary, Option<&Stream>) = match deref(doc, f) {
        Object::Dictionary(d) => (d, None),
        Object::Stream(s) => (&s.dict, Some(s)),
        Object::Array(fs) => {
            // An array of 1-output functions, one per component.
            let mut out = Vec::new();
            for sub in fs {
                out.extend(eval_function(doc, sub, x, depth + 1)?);
            }
            return Some(out);
        }
        _ => return None,
    };
    let floats = |key: &[u8]| -> Option<Vec<f64>> {
        match deref(doc, dict.get(key).ok()?) {
            Object::Array(a) => Some(a.iter().filter_map(|o| num(deref(doc, o))).collect()),
            _ => None,
        }
    };
    let ftype = dict.get(b"FunctionType").ok().and_then(|o| o.as_i64().ok())?;
    let out = match ftype {
        2 => {
            let c0 = floats(b"C0").unwrap_or_else(|| vec![0.0]);
            let c1 = floats(b"C1").unwrap_or_else(|| vec![1.0]);
            let n = dict.get(b"N").ok().and_then(|o| num(deref(doc, o))).unwrap_or(1.0);
            let t = x.powf(n);
            c0.iter().zip(c1.iter()).map(|(a, b)| a + t * (b - a)).collect()
        }
        0 => {
            // Sampled: take the sample nearest the input (1-input functions).
            let s = stream?;
            let data = s.decompressed_content().unwrap_or_else(|_| s.content.clone());
            let size = floats(b"Size")?.first().copied()? as usize;
            let bps = dict.get(b"BitsPerSample").ok().and_then(|o| o.as_i64().ok())? as usize;
            let range = floats(b"Range")?;
            let outputs = range.len() / 2;
            let decode = floats(b"Decode").unwrap_or_else(|| range.clone());
            let domain = floats(b"Domain").unwrap_or_else(|| vec![0.0, 1.0]);
            let encode = floats(b"Encode").unwrap_or_else(|| vec![0.0, (size - 1) as f64]);
            let xt = (x - domain[0]) / (domain[1] - domain[0]).max(1e-9);
            let idx = (encode[0] + xt * (encode[1] - encode[0])).round().clamp(0.0, (size - 1) as f64) as usize;
            let max = ((1u64 << bps) - 1) as f64;
            let mut out = Vec::with_capacity(outputs);
            for j in 0..outputs {
                let bit = (idx * outputs + j) * bps;
                let mut v: u64 = 0;
                for b in 0..bps {
                    let byte = *data.get((bit + b) / 8)?;
                    v = (v << 1) | ((byte >> (7 - (bit + b) % 8)) & 1) as u64;
                }
                let d0 = decode[2 * j];
                let d1 = decode[2 * j + 1];
                out.push(d0 + (v as f64 / max) * (d1 - d0));
            }
            out
        }
        3 => {
            // Stitching: pick the sub-function whose interval holds x.
            let fns = match deref(doc, dict.get(b"Functions").ok()?) {
                Object::Array(a) => a.clone(),
                _ => return None,
            };
            let domain = floats(b"Domain").unwrap_or_else(|| vec![0.0, 1.0]);
            let bounds = floats(b"Bounds").unwrap_or_default();
            let encode = floats(b"Encode").unwrap_or_default();
            let mut i = 0;
            while i < bounds.len() && x >= bounds[i] {
                i += 1;
            }
            let lo = if i == 0 { domain[0] } else { bounds[i - 1] };
            let hi = if i < bounds.len() { bounds[i] } else { domain[1] };
            let (e0, e1) = (*encode.get(2 * i).unwrap_or(&0.0), *encode.get(2 * i + 1).unwrap_or(&1.0));
            let xe = e0 + (x - lo) / (hi - lo).max(1e-9) * (e1 - e0);
            eval_function(doc, fns.get(i)?, xe, depth + 1)?
        }
        4 => {
            let s = stream?;
            let code = s.decompressed_content().unwrap_or_else(|_| s.content.clone());
            run_calculator(&String::from_utf8_lossy(&code), x)?
        }
        _ => return None,
    };
    // Clip to Range when given.
    Some(match floats(b"Range") {
        Some(r) if r.len() >= out.len() * 2 => {
            out.iter().enumerate().map(|(i, v)| v.clamp(r[2 * i], r[2 * i + 1])).collect()
        }
        _ => out,
    })
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// Minimal PostScript calculator (type 4 functions) for the arithmetic and
/// stack operators tint transforms use in practice. Conditionals aren't
/// supported; such functions fall back to a placeholder swatch.
fn run_calculator(code: &str, x: f64) -> Option<Vec<f64>> {
    let body = code.trim().strip_prefix('{')?.strip_suffix('}')?;
    if body.contains('{') {
        return None;
    }
    let mut st: Vec<f64> = vec![x];
    for tok in body.split_whitespace() {
        if let Ok(v) = tok.parse::<f64>() {
            st.push(v);
            continue;
        }
        match tok {
            "dup" => st.push(*st.last()?),
            "pop" => {
                st.pop()?;
            }
            "exch" => {
                let n = st.len();
                if n < 2 {
                    return None;
                }
                st.swap(n - 1, n - 2);
            }
            "copy" => {
                let n = st.pop()? as usize;
                let len = st.len();
                st.extend_from_within(len.checked_sub(n)?..);
            }
            "index" => {
                let n = st.pop()? as usize;
                st.push(*st.get(st.len().checked_sub(n + 1)?)?);
            }
            "roll" => {
                let j = st.pop()? as i64;
                let n = st.pop()? as usize;
                let len = st.len();
                let slice = &mut st[len.checked_sub(n)?..];
                if n > 0 {
                    let k = j.rem_euclid(n as i64) as usize;
                    slice.rotate_right(k);
                }
            }
            "add" | "sub" | "mul" | "div" | "max" | "min" => {
                let b = st.pop()?;
                let a = st.pop()?;
                st.push(match tok {
                    "add" => a + b,
                    "sub" => a - b,
                    "mul" => a * b,
                    "div" => a / b,
                    "max" => a.max(b),
                    _ => a.min(b),
                });
            }
            "neg" | "abs" | "floor" | "ceiling" | "round" | "truncate" | "cvr" | "sqrt" => {
                let a = st.pop()?;
                st.push(match tok {
                    "neg" => -a,
                    "abs" => a.abs(),
                    "floor" => a.floor(),
                    "ceiling" => a.ceil(),
                    "round" => a.round(),
                    "truncate" => a.trunc(),
                    "sqrt" => a.sqrt(),
                    _ => a,
                });
            }
            _ => return None,
        }
    }
    Some(st)
}

fn to_hex(r: f64, g: f64, b: f64) -> String {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
}

/// CIE L*a*b* (D50) to sRGB.
fn lab_to_rgb(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let finv = |t: f64| if t.powi(3) > 0.008856 { t.powi(3) } else { (t - 16.0 / 116.0) / 7.787 };
    // D50 white, then Bradford-adapted D50 -> linear sRGB.
    let (x, y, z) = (0.9642 * finv(fx), finv(fy), 0.8249 * finv(fz));
    let r = 3.1338561 * x - 1.6168667 * y - 0.4906146 * z;
    let g = -0.9787684 * x + 1.9161415 * y + 0.0334540 * z;
    let bl = 0.0719453 * x - 0.2289914 * y + 1.4052427 * z;
    let gamma = |c: f64| if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.max(0.0).powf(1.0 / 2.4) - 0.055 };
    (gamma(r), gamma(g), gamma(bl))
}

/// Converts alternate-space values to a hex swatch colour.
fn alt_to_hex(doc: &Document, alt: &Object, v: &[f64]) -> Option<String> {
    let family: Vec<u8> = match deref(doc, alt) {
        Object::Name(n) => n.clone(),
        Object::Array(a) => a.first().and_then(|o| deref(doc, o).as_name().ok())?.to_vec(),
        _ => return None,
    };
    let comps = match family.as_slice() {
        b"ICCBased" => match deref(doc, alt) {
            Object::Array(a) => a
                .get(1)
                .and_then(|o| deref(doc, o).as_stream().ok())
                .and_then(|s| s.dict.get(b"N").ok().and_then(|n| n.as_i64().ok()))
                .unwrap_or(0) as usize,
            _ => 0,
        },
        _ => 0,
    };
    match (family.as_slice(), comps) {
        (b"DeviceCMYK" | b"CMYK", _) | (b"ICCBased", 4) if v.len() >= 4 => {
            let k = 1.0 - v[3];
            Some(to_hex((1.0 - v[0]) * k, (1.0 - v[1]) * k, (1.0 - v[2]) * k))
        }
        (b"DeviceRGB" | b"RGB" | b"CalRGB", _) | (b"ICCBased", 3) if v.len() >= 3 => Some(to_hex(v[0], v[1], v[2])),
        (b"DeviceGray" | b"G" | b"CalGray", _) | (b"ICCBased", 1) if !v.is_empty() => Some(to_hex(v[0], v[0], v[0])),
        (b"Lab", _) if v.len() >= 3 => {
            let (r, g, b) = lab_to_rgb(v[0], v[1], v[2]);
            Some(to_hex(r, g, b))
        }
        _ => None,
    }
}

fn collect(doc: &Document, obj: &Object, depth: u32, out: &mut BTreeMap<String, String>) {
    if depth > 12 {
        return;
    }
    match obj {
        Object::Array(arr) => {
            if arr.first().and_then(|o| o.as_name().ok()) == Some(b"Separation".as_slice()) && arr.len() >= 4 {
                if let Ok(name) = deref(doc, &arr[1]).as_name() {
                    let name = String::from_utf8_lossy(name).into_owned();
                    if !out.contains_key(&name) {
                        if let Some(hex) = eval_function(doc, &arr[3], 1.0, 0).and_then(|v| alt_to_hex(doc, &arr[2], &v)) {
                            out.insert(name, hex);
                        }
                    }
                }
            }
            for o in arr {
                collect(doc, o, depth + 1, out);
            }
        }
        Object::Dictionary(d) => d.iter().for_each(|(_, v)| collect(doc, v, depth + 1, out)),
        Object::Stream(s) => s.dict.iter().for_each(|(_, v)| collect(doc, v, depth + 1, out)),
        Object::Reference(_) if depth == 0 => {}
        _ => {}
    }
}

/// Spot colour name → swatch colour ("#rrggbb") at 100% tint, for every
/// Separation colour space in the file whose colour could be worked out.
/// DeviceN colorants are covered through their /Colorants Separation arrays.
pub fn spot_swatches(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open PDF: {e}"))?;
    let mut out = BTreeMap::new();
    for obj in doc.objects.values() {
        collect(&doc, obj, 0, &mut out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculator() {
        // InDesign-style CMYK tint transform for C0 M100 Y81 K4.
        let v = run_calculator("{dup 0.0 mul exch dup 1.0 mul exch dup 0.81 mul exch 0.04 mul}", 1.0).unwrap();
        assert_eq!(v.len(), 4);
        assert!((v[1] - 1.0).abs() < 1e-9 && (v[2] - 0.81).abs() < 1e-9 && (v[3] - 0.04).abs() < 1e-9);
    }

    #[test]
    fn lab_white_and_red() {
        let (r, g, b) = lab_to_rgb(100.0, 0.0, 0.0);
        assert!(r > 0.98 && g > 0.98 && b > 0.98);
        let (r, g, b) = lab_to_rgb(50.0, 70.0, 50.0);
        assert!(r > 0.7 && g < 0.3 && b < 0.3);
    }

    /// `MP_TEST_PDF=path cargo test swatch_file -- --nocapture`
    #[test]
    fn swatch_file() {
        let Ok(p) = std::env::var("MP_TEST_PDF") else { return };
        println!("{:?}", spot_swatches(Path::new(&p)));
    }
}
