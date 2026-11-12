//! rf-win: Windows platform layer (fast enumeration, Shell COM, icons/thumbnails, file ops, watcher).
//!
//! Every function here may block (disk, network, shell extensions). Never call them on the UI thread.

pub mod accent;
pub mod apppkg;
pub mod apps;
pub mod bench;
pub mod cert;
pub mod clipboard;
pub mod complete;
pub mod ctxmenu;
pub mod default_fm;
pub mod design;
pub mod dialog_reg;
pub mod dialogs;
pub mod diskusage;
pub mod elf;
pub mod dnd;
pub mod drives;
pub mod encoding;
pub mod fastcopy;
pub mod fastdelete;
pub mod fileops;
pub mod foldersize;
pub mod sysparams;
pub mod git;
pub mod hotkey;
pub mod hash;
pub mod indexer;
pub mod known;
pub mod libraries;
pub mod ftp;
pub mod keys;
pub mod look;
pub mod listing;
pub mod localized;
pub mod nameindex;
pub mod taskbar_progress;
pub mod usn;
pub mod network;
pub mod pe;
pub mod security;
pub mod signature;
pub mod compat;
pub mod folder_custom;
pub mod fileactions;
pub mod fileinfo;
pub mod journal;
pub mod jumplist;
pub mod wsearch;
pub mod preview_host;
pub mod props;
pub mod recycle;
pub mod search;
pub mod shell;
pub mod shellfolder;
pub mod shortcut;
pub mod single_instance;
pub mod shell_window;
pub mod sta;
pub mod removal;
pub mod rotate;
pub mod thumbs;
pub mod tray;
pub mod watch;
pub mod window;
pub mod volume;
pub mod tasklist;
pub mod wide;

pub use drives::{Drive, DriveKind, list_drives};
pub use known::{KnownFolder, known_folders};
pub use listing::{ListError, list_dir, stat_entry};

/// Milliseconds since this process was created (start-up timing).
pub fn process_age_ms() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut created, mut a, mut b, mut c) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut a, &mut b, &mut c).is_err() {
            return 0;
        }
        let now = GetSystemTimePreciseAsFileTime();
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        t(now).saturating_sub(t(created)) / 10_000
    }
}
