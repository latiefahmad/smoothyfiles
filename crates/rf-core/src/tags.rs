//! File tags (Files' "Tags"): colored labels assigned to paths, persisted as JSON.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagDef {
    pub id: u32,
    pub name: String,
    /// 0xRRGGBB
    pub color: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tagged {
    /// Original-case path (keys are lower-cased for case-insensitive lookup).
    pub path: PathBuf,
    pub tags: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TagStore {
    pub defs: Vec<TagDef>,
    /// Key: lower-cased absolute path.
    pub items: BTreeMap<String, Tagged>,
    pub next_id: u32,
}

impl Default for TagStore {
    fn default() -> Self {
        let defaults = [
            ("Kırmızı", 0xFF453A),
            ("Turuncu", 0xFF9F0A),
            ("Sarı", 0xFFD60A),
            ("Yeşil", 0x30D158),
            ("Mavi", 0x0A84FF),
            ("Mor", 0xBF5AF2),
            ("Gri", 0x8E8E93),
        ];
        Self {
            defs: defaults
                .iter()
                .enumerate()
                .map(|(i, (n, c))| TagDef { id: i as u32 + 1, name: n.to_string(), color: *c })
                .collect(),
            items: BTreeMap::new(),
            next_id: defaults.len() as u32 + 1,
        }
    }
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

impl TagStore {
    pub fn def(&self, id: u32) -> Option<&TagDef> {
        self.defs.iter().find(|d| d.id == id)
    }

    /// Creates a tag; returns its id.
    pub fn add(&mut self, name: &str, color: u32) -> u32 {
        let id = self.next_id.max(self.defs.iter().map(|d| d.id + 1).max().unwrap_or(1));
        self.next_id = id + 1;
        self.defs.push(TagDef { id, name: name.trim().to_string(), color });
        id
    }

    pub fn rename(&mut self, id: u32, name: &str) {
        if let Some(d) = self.defs.iter_mut().find(|d| d.id == id) {
            d.name = name.trim().to_string();
        }
    }

    pub fn recolor(&mut self, id: u32, color: u32) {
        if let Some(d) = self.defs.iter_mut().find(|d| d.id == id) {
            d.color = color;
        }
    }

    /// Deletes a tag and takes it off every item.
    pub fn remove(&mut self, id: u32) {
        self.defs.retain(|d| d.id != id);
        for v in self.items.values_mut() {
            v.tags.retain(|t| *t != id);
        }
        self.items.retain(|_, v| !v.tags.is_empty());
    }

    pub fn tags_of(&self, p: &Path) -> &[u32] {
        self.items.get(&key(p)).map(|v| v.tags.as_slice()).unwrap_or(&[])
    }

    pub fn has(&self, p: &Path, id: u32) -> bool {
        self.tags_of(p).contains(&id)
    }

    /// Adds `id` to all paths, or removes it from all when every path already has it.
    pub fn toggle(&mut self, paths: &[PathBuf], id: u32) {
        let all = !paths.is_empty() && paths.iter().all(|p| self.has(p, id));
        for p in paths {
            let k = key(p);
            let v = self.items.entry(k.clone()).or_insert_with(|| Tagged { path: p.clone(), tags: Vec::new() });
            if all {
                v.tags.retain(|t| *t != id);
            } else if !v.tags.contains(&id) {
                v.tags.push(id);
            }
            if v.tags.is_empty() {
                self.items.remove(&k);
            }
        }
    }

    pub fn paths_with(&self, id: u32) -> Vec<PathBuf> {
        self.items.values().filter(|v| v.tags.contains(&id)).map(|v| v.path.clone()).collect()
    }

    /// Keep tags when a file is renamed/moved inside the app.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        if let Some(mut v) = self.items.remove(&key(from)) {
            v.path = to.to_path_buf();
            self.items.insert(key(to), v);
        }
    }

    pub fn load(path: &Path) -> Self {
        let text = match crate::read_config(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(_) => {
                crate::backup_corrupt(path);
                return Self::default();
            }
        };
        match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(store) => store,
            Err(_) => {
                // Unreadable: keep it aside instead of letting the next save overwrite the
                // user's tags with an empty store.
                crate::backup_corrupt(path);
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        static LOCK: parking_lot::Mutex<u64> = parking_lot::Mutex::new(0);
        let mut n = LOCK.lock();
        *n += 1;
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), *n));
        // never replace the real file with an empty one if serialization fails
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_and_query() {
        let mut s = TagStore::default();
        let a = PathBuf::from(r"C:\A.txt");
        let b = PathBuf::from(r"C:\b.txt");
        s.toggle(&[a.clone()], 1);
        assert!(s.has(Path::new(r"c:\a.TXT"), 1));
        s.toggle(&[a.clone(), b.clone()], 1); // b lacks it → add to both
        assert!(s.has(&b, 1));
        s.toggle(&[a.clone(), b.clone()], 1); // all have → remove
        assert!(!s.has(&a, 1) && !s.has(&b, 1));
        assert!(s.items.is_empty());
        s.toggle(&[a.clone()], 2);
        s.moved(&a, Path::new(r"C:\x.txt"));
        assert_eq!(s.paths_with(2), vec![PathBuf::from(r"C:\x.txt")]);
        let json = serde_json::to_string(&s).unwrap();
        let back: TagStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.defs.len(), 7);
    }
}
