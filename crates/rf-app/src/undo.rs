//! Undo (Ctrl+Z) and redo (Ctrl+Y) for operations done in smoothyfiles. The shell's own undo
//! stack is not scriptable, so we record what each operation did and run the inverse.

use crate::t;
use std::path::PathBuf;

use gpui_kit::*;
use parking_lot::Mutex;
use rf_win::fileops::Transfer;

#[derive(Debug, Clone)]
pub enum UndoOp {
    Renamed { from: PathBuf, to: PathBuf },
    Created { path: PathBuf },
    /// (source, result) pairs reported by the shell.
    Transferred { kind: Transfer, pairs: Vec<(PathBuf, PathBuf)> },
    /// (original path, path inside the Recycle Bin)
    Recycled { pairs: Vec<(PathBuf, PathBuf)> },
    /// "Flatten folder" on `dir`.
    Flattened { dir: PathBuf, rec: crate::ops::Flattened },
}

static STACK: Mutex<Vec<UndoOp>> = Mutex::new(Vec::new());
static REDO: Mutex<Vec<UndoOp>> = Mutex::new(Vec::new());
const MAX: usize = 50;

/// Records a new user operation (a new operation drops whatever could be redone).
pub fn push(op: UndoOp) {
    REDO.lock().clear();
    let mut s = STACK.lock();
    s.push(op);
    if s.len() > MAX {
        s.remove(0);
    }
}

/// Reverses the most recent operation (in the background, through the Status Center).
pub fn undo(cx: &mut App) {
    let Some(op) = STACK.lock().pop() else { return };
    // creating an item can't be replayed (its contents may have changed), everything else can
    if !matches!(op, UndoOp::Created { .. }) {
        REDO.lock().push(op.clone());
    }
    match op {
        UndoOp::Renamed { from, to } => {
            let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            crate::status_center::start(cx, format!("{} {name}", t!("Geri alınıyor: yeniden adlandırma →")), move |_| {
                rf_win::fileops::rename(&to, &name)
            });
        }
        UndoOp::Created { path } => crate::status_center::delete_ex(cx, vec![path], false, false),
        UndoOp::Transferred { kind: Transfer::Copy, pairs } => {
            crate::status_center::delete_ex(cx, pairs.into_iter().map(|(_, dst)| dst).collect(), false, false)
        }
        UndoOp::Transferred { kind: Transfer::Move, pairs } => {
            // Move everything back, grouped by original folder.
            let mut groups: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> = Default::default();
            for (src, dst) in pairs {
                if let Some(parent) = src.parent() {
                    groups.entry(parent.to_path_buf()).or_default().push(dst);
                }
            }
            for (dir, items) in groups {
                crate::status_center::transfer_ex(cx, Transfer::Move, items, dir, false);
            }
        }
        UndoOp::Recycled { pairs } => {
            let n = pairs.len();
            crate::status_center::start(cx, format!("{} {}", t!("Geri dönüşüm kutusundan geri yükleniyor:"), crate::i18n::items(n)), move |_| {
                let wanted: Vec<String> = pairs.iter().map(|(_, bin)| bin.to_string_lossy().to_lowercase()).collect();
                let items: Vec<_> = rf_win::recycle::list()?
                    .into_iter()
                    .filter(|it| wanted.contains(&it.parsing_path.to_lowercase()))
                    .collect();
                anyhow::ensure!(!items.is_empty(), t!("öğeler geri dönüşüm kutusunda bulunamadı"));
                rf_win::recycle::restore(&items)?;
                Ok(true)
            });
        }
        UndoOp::Flattened { dir, rec } => {
            let name = rf_core::history::display_name(&dir);
            crate::status_center::start(cx, format!("{} {name}", t!("Geri alınıyor: düzleştirme →")), move |_| crate::ops::unflatten(&rec));
        }
    }
}

/// Repeats the last undone operation.
pub fn redo(cx: &mut App) {
    let Some(op) = REDO.lock().pop() else { return };
    // it becomes undoable again
    STACK.lock().push(op.clone());
    match op {
        UndoOp::Renamed { from, to } => {
            let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            crate::status_center::start(cx, format!("{} {name}", t!("Yineleniyor: yeniden adlandırma →")), move |_| {
                rf_win::fileops::rename(&from, &name)
            });
        }
        UndoOp::Created { .. } => {}
        UndoOp::Transferred { kind, pairs } => {
            let mut groups: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> = Default::default();
            for (src, dst) in pairs {
                if let Some(parent) = dst.parent() {
                    groups.entry(parent.to_path_buf()).or_default().push(src);
                }
            }
            for (dir, items) in groups {
                crate::status_center::transfer_ex(cx, kind, items, dir, false);
            }
        }
        UndoOp::Recycled { pairs } => {
            crate::status_center::delete_ex(cx, pairs.into_iter().map(|(orig, _)| orig).collect(), false, false)
        }
        UndoOp::Flattened { dir, rec } => {
            let name = rf_core::history::display_name(&dir);
            crate::status_center::start(cx, format!("{} {name}", t!("Yineleniyor: düzleştirme →")), move |_| crate::ops::reflatten(&rec));
        }
    }
}
