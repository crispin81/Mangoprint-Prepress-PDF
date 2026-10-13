//! Undo / redo: a copy of a working PDF is kept before every edit, so
//! Ctrl+Z can step back through them (and Ctrl+Y forward again).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

/// How many steps back are kept for each open PDF.
const MAX_STEPS: usize = 50;

struct Steps {
    dir: TempDir,
    undo: Vec<PathBuf>,
    redo: Vec<PathBuf>,
    next: u64,
}

impl Steps {
    fn file(&mut self) -> PathBuf {
        self.next += 1;
        self.dir.path().join(format!("{}.pdf", self.next))
    }
}

#[derive(serde::Serialize)]
pub struct Counts {
    pub undo: usize,
    pub redo: usize,
}

/// Saved steps per working copy path.
pub struct History(Mutex<HashMap<String, Steps>>);

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => std::fs::read(a).ok() == std::fs::read(b).ok(),
        _ => false,
    }
}

impl History {
    pub fn new() -> Self {
        History(Mutex::new(HashMap::new()))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Steps>>, String> {
        self.0.lock().map_err(|_| "Undo history lock was poisoned".to_string())
    }

    /// Keeps a copy of `path` as it is now, before an edit.
    pub fn snapshot(&self, path: &str) -> Result<(), String> {
        let mut map = self.lock()?;
        if !map.contains_key(path) {
            let dir = TempDir::new().map_err(|e| format!("Could not create a temp directory: {e}"))?;
            map.insert(path.to_string(), Steps { dir, undo: Vec::new(), redo: Vec::new(), next: 0 });
        }
        let steps = map.get_mut(path).expect("just inserted");
        let f = steps.file();
        std::fs::copy(path, &f).map_err(|e| format!("Could not save an undo step: {e}"))?;
        steps.undo.push(f);
        for old in steps.redo.drain(..) {
            let _ = std::fs::remove_file(old);
        }
        while steps.undo.len() > MAX_STEPS {
            let _ = std::fs::remove_file(steps.undo.remove(0));
        }
        Ok(())
    }

    /// Steps back (`undo`) or forward (`!undo`). Returns false when there
    /// was nothing to do. An edit that failed leaves a step identical to
    /// the file; those are skipped.
    pub fn step(&self, path: &str, undo: bool) -> Result<bool, String> {
        let mut map = self.lock()?;
        let Some(steps) = map.get_mut(path) else { return Ok(false) };
        let cur = Path::new(path);
        loop {
            let popped = if undo { steps.undo.pop() } else { steps.redo.pop() };
            let Some(f) = popped else { return Ok(false) };
            if same_file(&f, cur) {
                let _ = std::fs::remove_file(f);
                continue;
            }
            // Keep the current state on the other stack, then restore.
            let keep = steps.file();
            std::fs::copy(cur, &keep).map_err(|e| format!("Could not save an undo step: {e}"))?;
            if undo { steps.redo.push(keep) } else { steps.undo.push(keep) }
            std::fs::copy(&f, cur).map_err(|e| format!("Could not restore the PDF: {e}"))?;
            let _ = std::fs::remove_file(f);
            return Ok(true);
        }
    }

    pub fn counts(&self, path: &str) -> Counts {
        self.lock()
            .ok()
            .and_then(|m| m.get(path).map(|s| Counts { undo: s.undo.len(), redo: s.redo.len() }))
            .unwrap_or(Counts { undo: 0, redo: 0 })
    }

    /// Drops a closed PDF's steps.
    pub fn forget(&self, path: &str) {
        if let Ok(mut m) = self.lock() {
            m.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.pdf");
        let ps = p.to_string_lossy().into_owned();
        let h = History::new();
        std::fs::write(&p, "one").unwrap();
        h.snapshot(&ps).unwrap();
        std::fs::write(&p, "two").unwrap();
        h.snapshot(&ps).unwrap(); // a failed edit: nothing changed
        h.snapshot(&ps).unwrap();
        std::fs::write(&p, "three").unwrap();
        assert!(h.step(&ps, true).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert!(h.step(&ps, true).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one");
        assert!(!h.step(&ps, true).unwrap());
        assert!(h.step(&ps, false).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert!(h.step(&ps, false).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "three");
        assert!(!h.step(&ps, false).unwrap());
    }
}
