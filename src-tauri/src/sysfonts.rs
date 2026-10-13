//! The computer's installed fonts, for mail merge text. Only TrueType and
//! OpenType files whose licence allows embedding are listed, since merged
//! text is always embedded in the PDF.

use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Serialize, Clone, Debug)]
pub struct SysFont {
    /// e.g. "Georgia".
    pub family: String,
    /// e.g. "Bold Italic".
    pub style: String,
    /// The font's PostScript name, which Ghostscript finds it by.
    pub ps_name: String,
    pub path: String,
}

/// Folders fonts are installed in on this platform.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    #[cfg(windows)]
    {
        let windir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        dirs.push(windir.join("Fonts"));
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(h) = &home {
            dirs.push(h.join("Library/Fonts"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(h) = &home {
            dirs.push(h.join(".local/share/fonts"));
            dirs.push(h.join(".fonts"));
        }
    }
    let _ = &home;
    dirs
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 4 {
                walk(&p, depth + 1, out);
            }
        } else if matches!(p.extension().and_then(|x| x.to_str()).map(|x| x.to_ascii_lowercase()).as_deref(), Some("ttf" | "otf")) {
            out.push(p);
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]))
}

fn read_table(f: &mut File, offset: u32, len: u32) -> Option<Vec<u8>> {
    if len > 4 * 1024 * 1024 {
        return None;
    }
    f.seek(SeekFrom::Start(offset as u64)).ok()?;
    let mut buf = vec![0; len as usize];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// The best string for name `id` in a 'name' table: Windows English
/// first, then any Windows Unicode, then Mac Roman.
fn name(table: &[u8], id: u16) -> Option<String> {
    let count = u16_at(table, 2)? as usize;
    let strings = u16_at(table, 4)? as usize;
    let mut best: Option<(u8, String)> = None;
    for r in 0..count {
        let at = 6 + r * 12;
        let (platform, encoding, lang, nid) = (u16_at(table, at)?, u16_at(table, at + 2)?, u16_at(table, at + 4)?, u16_at(table, at + 6)?);
        if nid != id {
            continue;
        }
        let (len, off) = (u16_at(table, at + 8)? as usize, u16_at(table, at + 10)? as usize);
        let raw = table.get(strings + off..strings + off + len)?;
        let (rank, text) = match platform {
            3 if encoding == 1 || encoding == 10 => {
                let units: Vec<u16> = raw.chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                (if lang == 0x409 { 0 } else { 1 }, String::from_utf16_lossy(&units))
            }
            0 => {
                let units: Vec<u16> = raw.chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                (2, String::from_utf16_lossy(&units))
            }
            1 if encoding == 0 => (3, raw.iter().map(|&b| b as char).collect()),
            _ => continue,
        };
        if best.as_ref().map_or(true, |(r, _)| rank < *r) {
            best = Some((rank, text));
        }
    }
    best.map(|(_, t)| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// Reads one font file's names, or None if it can't be used (unreadable,
/// a collection, or a licence that forbids embedding).
pub fn read_font(path: &Path) -> Option<SysFont> {
    let mut f = File::open(path).ok()?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head).ok()?;
    let version = u32_at(&head, 0)?;
    if version != 0x0001_0000 && &head[0..4] != b"OTTO" && &head[0..4] != b"true" {
        return None;
    }
    let num = u16_at(&head, 4)? as usize;
    let mut dir = vec![0u8; num * 16];
    f.read_exact(&mut dir).ok()?;
    let mut name_t = None;
    let mut os2_t = None;
    for i in 0..num {
        let at = i * 16;
        let tag = &dir[at..at + 4];
        let (off, len) = (u32_at(&dir, at + 8)?, u32_at(&dir, at + 12)?);
        if tag == b"name" {
            name_t = Some((off, len));
        } else if tag == b"OS/2" {
            os2_t = Some((off, len));
        }
    }
    if let Some((off, len)) = os2_t {
        let os2 = read_table(&mut f, off, len.min(64))?;
        let fs_type = u16_at(&os2, 8).unwrap_or(0);
        // Restricted licence, or bitmap-only embedding: can't embed.
        if fs_type & 0x000F == 0x0002 || fs_type & 0x0200 != 0 {
            return None;
        }
    }
    let (off, len) = name_t?;
    let table = read_table(&mut f, off, len)?;
    let ps_name = name(&table, 6)?;
    // PostScript names are plain ASCII without spaces; anything else won't
    // be found by Ghostscript.
    if !ps_name.bytes().all(|b| b.is_ascii_graphic() && !b"[](){}<>/%".contains(&b)) {
        return None;
    }
    let family = name(&table, 16).or_else(|| name(&table, 1))?;
    let style = name(&table, 17).or_else(|| name(&table, 2)).unwrap_or_else(|| "Regular".into());
    Some(SysFont { family, style, ps_name, path: path.to_string_lossy().into_owned() })
}

/// Every usable installed font, sorted by family then style, one entry per
/// PostScript name.
pub fn list() -> Vec<SysFont> {
    let mut files = Vec::new();
    for d in font_dirs() {
        walk(&d, 0, &mut files);
    }
    let mut fonts: Vec<SysFont> = files.iter().filter_map(|p| read_font(p)).collect();
    fonts.sort_by(|a, b| (a.family.to_lowercase(), a.style.to_lowercase()).cmp(&(b.family.to_lowercase(), b.style.to_lowercase())));
    fonts.dedup_by(|a, b| a.ps_name == b.ps_name);
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_installed_fonts() {
        let fonts = list();
        println!("{} fonts, e.g. {:?}", fonts.len(), fonts.iter().take(3).collect::<Vec<_>>());
        for f in &fonts {
            assert!(!f.ps_name.contains(' '));
        }
    }
}
