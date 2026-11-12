//! rf-core: platform-independent model for smoothyfiles (entries, sorting, filtering, navigation).

pub mod entry;
pub mod format;
pub mod history;
pub mod rename;
pub mod shelf;
pub mod sort;
pub mod tags;
pub mod treemap;

pub use entry::{Entry, EntryKind, attrs};
pub use history::{History, Location};
pub use sort::{SortDir, SortKey, SortSpec};

/// Renames an unreadable settings/data file to `<name>.bozuk-<unix secs>` so a later save
/// cannot silently destroy what the user had (they can fix and restore it). Best effort.
pub fn backup_corrupt(path: &std::path::Path) {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".bozuk-{ms}"));
    let _ = std::fs::rename(path, path.with_file_name(name));
}

/// Reads a small config file, retrying briefly on transient errors (antivirus / sync tools
/// holding it open) so a passing lock is not mistaken for a broken file.
pub fn read_config(path: &std::path::Path) -> std::io::Result<String> {
    let mut last = None;
    for attempt in 0..3 {
        match std::fs::read_to_string(path) {
            Ok(t) => return Ok(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::InvalidData => return Err(e),
            Err(e) => {
                last = Some(e);
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("read failed")))
}
pub mod textload;
