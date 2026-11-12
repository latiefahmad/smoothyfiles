//! The shelf: a staging list of items collected from different folders, to copy/move/delete
//! them together later (Files' "Shelf pane"). Ordered, unique (case-insensitive on Windows).

use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct Shelf {
    items: Vec<PathBuf>,
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

impl Shelf {
    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn contains(&self, p: &Path) -> bool {
        let k = key(p);
        self.items.iter().any(|i| key(i) == k)
    }
    /// Adds paths not already on the shelf; returns how many were added.
    pub fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let before = self.items.len();
        for p in paths {
            if !self.contains(&p) {
                self.items.push(p);
            }
        }
        self.items.len() - before
    }
    pub fn remove(&mut self, p: &Path) {
        let k = key(p);
        self.items.retain(|i| key(i) != k);
    }
    /// Drops items that no longer exist (moved/deleted elsewhere). Blocking (stats each item).
    pub fn prune_missing(&mut self) {
        self.items.retain(|p| p.exists());
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
    /// After a move: replaces moved items with their new locations.
    pub fn relocate(&mut self, from: &Path, to: &Path) {
        let k = key(from);
        for i in &mut self.items {
            if key(i) == k {
                *i = to.to_path_buf();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_ordered() {
        let mut s = Shelf::default();
        assert_eq!(s.add([PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\B")]), 2);
        assert_eq!(s.add([PathBuf::from(r"c:\A.TXT"), PathBuf::from(r"C:\c")]), 1);
        assert_eq!(s.items(), &[PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\B"), PathBuf::from(r"C:\c")]);
        s.remove(Path::new(r"c:\b"));
        assert_eq!(s.len(), 2);
        s.relocate(Path::new(r"C:\c"), Path::new(r"D:\c"));
        assert!(s.contains(Path::new(r"d:\C")));
    }
}
