//! Shell folders without a file-system path (an MTP phone, Phone Link's mobile devices):
//! copying items out and the notice for what a first, read-only version leaves out.

use std::path::PathBuf;

use rf_win::shellfolder::ShellChild;

use crate::t;

/// A short message in the Status Center: what was asked for cannot be done in a shell folder.
pub fn read_only_notice(cx: &mut gpui_kit::App) {
    crate::status_center::start(cx, t!("Bu klasör salt okunur").to_string(), |_| {
        Err(anyhow::anyhow!(t!("Telefon gibi cihazlardaki öğeler burada değiştirilemez; kopyalayıp başka bir klasöre yapıştırabilirsiniz.")))
    });
}

/// Ctrl+C on items of a shell folder: they are copied into a fresh temp folder with the shell's
/// copy engine (Status Center shows it), then those copies go on the clipboard. The clipboard is
/// emptied at once so a quick paste cannot paste what was there before, and what the user
/// copies meanwhile is left alone. Items that are real files go on the clipboard as they are.
pub fn copy_items(cx: &mut gpui_kit::App, items: Vec<ShellChild>) {
    if items.is_empty() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    rf_win::sta::run(move |owner| {
        if let Err(e) = rf_win::clipboard::clear(owner) {
            log::warn!("clipboard: {e:#}");
        }
        let _ = tx.send(rf_win::clipboard::sequence());
    });
    let names = items.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
    let title = format!("{} {}", t!("Cihazdan kopyalanıyor:"), names);
    crate::status_center::start(cx, title, move |_| {
        let seq = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(0);
        let real = copy_to_temp(&items)?;
        if real.is_empty() {
            return Ok(false);
        }
        if rf_win::clipboard::sequence() != seq {
            return Ok(true);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        rf_win::sta::run(move |owner| {
            let _ = tx.send(rf_win::clipboard::set_files(owner, &real, rf_win::fileops::Transfer::Copy));
        });
        rx.recv().map_err(|_| anyhow::anyhow!("clipboard"))??;
        Ok(true)
    });
}

impl crate::folder_view::FolderView {
    /// Right-click in a shell folder (a phone, Libraries…): the shell's own menu for the selected
    /// items, found by their parsing names (they have no path), or for the folder's background.
    /// The shell carries out what is chosen (copy, delete on the device, properties…); the
    /// listing is read again afterwards.
    pub fn show_shell_folder_menu(&mut self, background: bool, cx: &mut gpui_kit::Context<Self>) {
        let rf_core::history::Location::Shell { parsing, .. } = self.location().clone() else { return };
        let mut sel: Vec<u32> = self.selected.iter().copied().collect();
        sel.sort_unstable();
        let items: Vec<PathBuf> = if background { Vec::new() } else { sel.iter().filter_map(|&i| self.shell_items.get(i as usize)).map(|c| PathBuf::from(&c.parsing)).collect() };
        let (tx, rx) = futures::channel::oneshot::channel();
        rf_win::sta::run(move |owner| {
            let mut pt = windows::Win32::Foundation::POINT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
            }
            let r = match items.as_slice() {
                [] => rf_win::ctxmenu::show_for_background(owner, std::path::Path::new(&parsing), (pt.x, pt.y), false, &[]),
                [one] => rf_win::ctxmenu::show_for_one(owner, one, (pt.x, pt.y), false, &[]),
                many => rf_win::ctxmenu::show_for_items(owner, many, (pt.x, pt.y), false, &[]),
            };
            let _ = tx.send(r.map_err(|e| format!("{e:#}")));
        });
        cx.spawn(async move |this, cx| {
            match rx.await {
                Ok(Ok(rf_win::ctxmenu::MenuOutcome::Dismissed)) | Err(_) => {}
                Ok(Ok(_)) => {
                    // the device may have changed (a deletion, a new folder): list it again
                    let _ = this.update(cx, |v, cx| v.refresh(cx));
                }
                Ok(Err(e)) => log::info!("shell folder menu: {e}"),
            }
        })
        .detach();
    }
}

/// Real files for the items: file-system items as they are, the others copied (blocking) into
/// a new folder under the temp root that `archive_view::clean_temp` clears after two days.
fn copy_to_temp(items: &[ShellChild]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = items.iter().filter_map(|c| c.fs_path.clone()).collect();
    let virtual_items: Vec<String> = items.iter().filter(|c| c.fs_path.is_none()).map(|c| c.parsing.clone()).collect();
    if virtual_items.is_empty() {
        return Ok(out);
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dest = crate::archive_view::temp_root().join(format!("cihaz-{stamp:x}"));
    std::fs::create_dir_all(&dest)?;
    if !rf_win::shellfolder::copy_out(&virtual_items, &dest)? {
        return Ok(Vec::new());
    }
    // what arrived: the shell names the copies, so the folder is read rather than guessed at
    for e in std::fs::read_dir(&dest)? {
        out.push(e?.path());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Items inside a `.zip` have no file-system path, like a phone's: `copy_to_temp` pulls them
    /// out into a temp folder and returns real files; items with a path pass through.
    #[test]
    fn items_without_a_path_become_real_files() {
        let dir = std::env::temp_dir().join(format!("rf-shellview-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("note.txt");
        std::fs::write(&src, b"abc").unwrap();
        let zip = dir.join("t.zip");
        let ok = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Compress-Archive -Path '{}' -DestinationPath '{}'", src.display(), zip.display())])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let mut kids = Vec::new();
        rf_win::shellfolder::list(&zip.to_string_lossy(), &|| false, &mut |b| kids.extend(b)).unwrap();
        let on_disk = ShellChild { name: "x".into(), parsing: src.to_string_lossy().into(), is_folder: false, fs_path: Some(src.clone()), size: None, modified_ms: None };
        kids.push(on_disk);
        let real = copy_to_temp(&kids).unwrap();
        assert_eq!(real.len(), 2, "{real:?}");
        assert!(real.iter().all(|p| p.is_file()), "{real:?}");
        assert!(real.contains(&src));
        let copy = real.iter().find(|p| **p != src).unwrap();
        assert_eq!(std::fs::read(copy).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(copy.parent().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
