//! QR codes and Data Matrix codes for mail merge.

use datamatrix::{DataMatrix, SymbolList};
use qrcode::{EcLevel, QrCode};
use serde::Serialize;

/// A 2D barcode as a grid of modules, row by row from the top.
#[derive(Serialize, Debug)]
pub struct Matrix {
    pub width: usize,
    pub height: usize,
    /// `true` = a dark module.
    pub bits: Vec<bool>,
    /// The clear margin the code needs around it, in modules.
    pub quiet: usize,
}

/// Encodes `text` as `kind`: "qr" or "datamatrix". GS1 Data Matrix
/// ("gs1datamatrix") takes element strings in the usual written form,
/// e.g. `(01)03453120000011(17)191125(10)ABCD1234`.
pub fn encode(kind: &str, text: &str) -> Result<Matrix, String> {
    match kind {
        "qr" => {
            let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)
                .map_err(|_| format!("\"{}\" is too long for a QR code.", short(text)))?;
            let width = code.width();
            let bits = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
            Ok(Matrix { width, height: width, bits, quiet: 4 })
        }
        "datamatrix" | "gs1datamatrix" => {
            let symbols = SymbolList::default().enforce_square();
            let code = if kind == "gs1datamatrix" {
                DataMatrix::encode_gs1(&gs1_data(text)?, symbols)
            } else {
                DataMatrix::encode_str(text, symbols.clone()).or_else(|_| DataMatrix::encode(text.as_bytes(), symbols))
            }
            .map_err(|_| format!("\"{}\" can't be made into a Data Matrix code (it may be too long).", short(text)))?;
            let bitmap = code.bitmap();
            Ok(Matrix { width: bitmap.width(), height: bitmap.height(), bits: bitmap.bits().to_vec(), quiet: 1 })
        }
        _ => Err(format!("Unknown barcode type {kind}.")),
    }
}

/// GS1 element strings with the brackets taken off the application
/// identifiers and a GS separator after each variable-length value.
/// Text without brackets is used as it is.
fn gs1_data(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if !text.starts_with('(') {
        return Ok(text.as_bytes().to_vec());
    }
    let mut elements = Vec::new();
    let mut rest = text;
    while let Some(after) = rest.strip_prefix('(') {
        let close = after.find(')').ok_or_else(|| format!("\"{}\" has a ( without a ).", short(text)))?;
        let value_end = after[close + 1..].find('(').map_or(after.len(), |i| close + 1 + i);
        elements.push((&after[..close], &after[close + 1..value_end]));
        rest = &after[value_end..];
    }
    let mut out = Vec::new();
    for (i, (ai, value)) in elements.iter().enumerate() {
        out.extend_from_slice(ai.as_bytes());
        out.extend_from_slice(value.as_bytes());
        if i + 1 < elements.len() && !fixed_length(ai) {
            out.push(0x1D);
        }
    }
    Ok(out)
}

/// GS1 identifiers whose values have a fixed length, so they need no GS
/// separator after them.
fn fixed_length(ai: &str) -> bool {
    ["00", "01", "02", "03", "04", "11", "12", "13", "14", "15", "16", "17", "18", "19", "20", "31", "32", "33", "34", "35", "36", "41"]
        .iter()
        .any(|p| ai.starts_with(p))
}

fn short(text: &str) -> String {
    let t: String = text.chars().take(30).collect();
    if t.len() < text.len() { format!("{t}…") } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_and_datamatrix() {
        let q = encode("qr", "https://mangoprint.co.uk").unwrap();
        assert_eq!(q.bits.len(), q.width * q.height);
        assert!(q.width >= 21);
        let d = encode("datamatrix", "Jack Davidson 07412 991111").unwrap();
        assert_eq!(d.width, d.height);
        assert_eq!(d.bits.len(), d.width * d.height);
        // The solid L: the left column and bottom row are all dark.
        assert!((0..d.height).all(|y| d.bits[y * d.width]));
        assert!((0..d.width).all(|x| d.bits[(d.height - 1) * d.width + x]));
        let back = DataMatrix::decode(&d.bits, d.width).unwrap();
        assert_eq!(back, b"Jack Davidson 07412 991111");
        assert_eq!(gs1_data("(01)03453120000011(10)AB12(17)191125").unwrap(), b"010345312000001110AB1217191125");
    }
}
