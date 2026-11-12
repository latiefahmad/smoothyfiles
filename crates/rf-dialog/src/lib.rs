//! smoothyfiles as the Open / Save dialog. This DLL is registered (per user, HKCU) as the COM
//! server of Windows' FileOpenDialog and FileSaveDialog classes, so apps that ask Windows for
//! the common file dialog load it.
//!
//! Each object wraps Windows' real dialog (taken straight from comdlg32.dll, whatever the
//! registry says): every call the app makes is recorded and passed on to it. `Show` asks
//! smoothyfiles (see `ipc`); if the app added its own controls to the dialog, or smoothyfiles
//! can't be reached, the real dialog is shown instead, already set up exactly as the app asked.

#![allow(non_snake_case)]

pub mod proto;
#[cfg(windows)]
mod ipc;

/// One line to `%LOCALAPPDATA%/smoothyfiles/dialog.log` (kept under 256 KB): which app asked,
/// whether smoothyfiles answered, and how the app read the result. For diagnosing apps.
pub(crate) fn log(line: &str) {
    use std::io::Write;
    let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
    let dir = std::path::PathBuf::from(base).join("smoothyfiles");
    let path = dir.join("dialog.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 << 10) {
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let app = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        let _ = writeln!(f, "{secs} {app}[{}] dll {}-{} {line}", std::process::id(), env!("CARGO_PKG_VERSION"), env!("SMOOTHYFILES_COMMIT"));
    }
}

#[cfg(windows)]
pub use com::*;

#[cfg(windows)]
mod com {
    use std::cell::RefCell;
    use std::ffi::c_void;

    use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOINTERFACE, E_POINTER, ERROR_CANCELLED, HWND, S_FALSE, S_OK};
    use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree, IClassFactory, IClassFactory_Impl};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::Shell::Common::{COMDLG_FILTERSPEC, ITEMIDLIST};
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyDescriptionList, IPropertyStore};
    use windows::Win32::UI::Shell::*;
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement, w};
    use windows_core::{BOOL as WBOOL, IUnknownImpl as _};

    use crate::proto::{Filter, Mode, Request};

    type DllGetClassObjectFn = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

    /// Windows' own dialog class, from comdlg32.dll directly (our HKCU registration would
    /// otherwise point CoCreateInstance back at us).
    fn real_dialog(clsid: &GUID) -> Result<IFileDialog> {
        unsafe {
            let lib = LoadLibraryW(w!("comdlg32.dll"))?;
            let f = GetProcAddress(lib, windows::core::s!("DllGetClassObject")).ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let f: DllGetClassObjectFn = std::mem::transmute(f);
            let mut factory: *mut c_void = std::ptr::null_mut();
            f(clsid, &IClassFactory::IID, &mut factory).ok()?;
            let factory = IClassFactory::from_raw(factory);
            factory.CreateInstance::<Option<&IUnknown>, IFileDialog>(None)
        }
    }

    fn wstr(p: &PCWSTR) -> String {
        if p.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } }
    }

    fn co_string(s: &str) -> Result<PWSTR> {
        let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let p = CoTaskMemAlloc(w.len() * 2) as *mut u16;
            if p.is_null() {
                return Err(E_FAIL.into());
            }
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            Ok(PWSTR(p))
        }
    }

    fn item_path(item: &IShellItem) -> Option<String> {
        unsafe {
            let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s
        }
    }

    /// A shell item for a path; a file being saved may not exist yet.
    fn item_for(path: &str) -> Result<IShellItem> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if let Ok(i) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None) {
                return Ok(i);
            }
            let pidl = SHSimpleIDListFromPath(PCWSTR(wide.as_ptr()));
            if pidl.is_null() {
                return Err(E_FAIL.into());
            }
            let r = SHCreateItemFromIDList::<IShellItem>(pidl);
            CoTaskMemFree(Some(pidl as *const c_void));
            r
        }
    }

    fn items_for(paths: &[String]) -> Result<IShellItemArray> {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let wide: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
            let pidl = unsafe { SHSimpleIDListFromPath(PCWSTR(wide.as_ptr())) };
            if !pidl.is_null() {
                pidls.push(pidl);
            }
        }
        let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const _).collect();
        let r = unsafe { SHCreateShellItemArrayFromIDLists(&consts) };
        for p in pidls {
            unsafe { CoTaskMemFree(Some(p as *const c_void)) };
        }
        r
    }

    fn exe_name() -> String {
        std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default()
    }

    #[derive(Default)]
    struct State {
        options: FILEOPENDIALOGOPTIONS,
        title: String,
        ok_label: String,
        filters: Vec<Filter>,
        type_index: u32,
        default_ext: String,
        folder: Option<String>,
        default_folder: Option<String>,
        file_name: String,
        /// The app added its own controls: only the real dialog can show them.
        customized: bool,
        sinks: Vec<(u32, IFileDialogEvents)>,
        /// Picked in smoothyfiles (None: not shown by us, the real dialog has the answer).
        result: Option<Vec<String>>,
    }

    #[implement(IFileOpenDialog, IFileSaveDialog, IFileDialogCustomize)]
    pub struct Dialog {
        save: bool,
        real: IFileDialog,
        st: RefCell<State>,
    }

    impl Dialog {
        fn new(save: bool) -> Result<Dialog> {
            let real = real_dialog(if save { &FileSaveDialog } else { &FileOpenDialog })?;
            // Windows' defaults (a Save dialog asks before overwriting): an app that never calls
            // SetOptions relies on them
            let options = unsafe { real.GetOptions() }.unwrap_or_default();
            Ok(Dialog { save, real, st: RefCell::new(State { type_index: 1, options, ..Default::default() }) })
        }

        fn request(&self, owner: HWND) -> Request {
            let st = self.st.borrow();
            let mode = if st.options.0 & FOS_PICKFOLDERS.0 != 0 {
                Mode::Folder
            } else if self.save {
                Mode::Save
            } else {
                Mode::Open
            };
            Request {
                mode,
                multi: st.options.0 & FOS_ALLOWMULTISELECT.0 != 0,
                title: st.title.clone(),
                ok_label: st.ok_label.clone(),
                folder: st.folder.clone().or_else(|| st.default_folder.clone()),
                file_name: st.file_name.clone(),
                filters: st.filters.clone(),
                filter_index: st.type_index.max(1),
                default_ext: st.default_ext.trim_start_matches('.').to_string(),
                overwrite_prompt: self.save && st.options.0 & FOS_OVERWRITEPROMPT.0 != 0,
                owner: owner.0 as isize,
                app: exe_name(),
            }
        }
    }

    impl IModalWindow_Impl for Dialog_Impl {
        fn Show(&self, owner: HWND) -> Result<()> {
            let off = std::env::var_os("SMOOTHYFILES_DIALOG_OFF").is_some();
            if off || self.st.borrow().customized {
                crate::log(&format!("show: Windows' dialog (customized={})", self.st.borrow().customized));
                return unsafe { self.real.Show(Some(owner)) };
            }
            let req = self.request(owner);
            let has_owner = !owner.is_invalid();
            {
                use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(owner, Some(&mut pid)) };
                let front = unsafe { GetForegroundWindow() } == owner;
                crate::log(&format!("show: {:?}, owner {:?} of process {pid}, owner in front: {front}", req.mode, owner.0));
            }
            if has_owner {
                unsafe {
                    let _ = EnableWindow(owner, false);
                }
            }
            let answer = crate::ipc::pick(&req, owner);
            if has_owner {
                unsafe {
                    let _ = EnableWindow(owner, true);
                    let _ = SetForegroundWindow(owner);
                }
            }
            let Some(resp) = answer else {
                // smoothyfiles couldn't help: Windows' dialog, set up as the app asked
                crate::log(&format!("show: no answer from smoothyfiles ({:?}), Windows' dialog", req.mode));
                return unsafe { self.real.Show(Some(owner)) };
            };
            if !resp.ok || resp.paths.is_empty() {
                crate::log("show: cancelled");
                return Err(ERROR_CANCELLED.to_hresult().into());
            }
            crate::log(&format!("show: {:?} picked {} item(s), {} event sink(s)", req.mode, resp.paths.len(), self.st.borrow().sinks.len()));
            {
                let mut st = self.st.borrow_mut();
                st.type_index = resp.filter_index.max(1);
                st.file_name = std::path::Path::new(&resp.paths[0]).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                st.result = Some(resp.paths);
            }
            // The app may still refuse the choice: only S_FALSE from OnFileOk does that (it ends
            // our dialog as cancelled). Anything else accepts, as in Windows' dialog: Chrome's
            // sinks answer E_NOTIMPL, which windows-rs would report as an error.
            let me: IFileDialog = self.to_interface::<IFileOpenDialog>().cast()?;
            let sinks: Vec<IFileDialogEvents> = self.st.borrow().sinks.iter().map(|(_, s)| s.clone()).collect();
            for s in sinks {
                let hr = unsafe { (Interface::vtable(&s).OnFileOk)(Interface::as_raw(&s), Interface::as_raw(&me)) };
                if hr == S_FALSE {
                    crate::log("show: the app refused the choice in OnFileOk (S_FALSE)");
                    self.st.borrow_mut().result = None;
                    return Err(ERROR_CANCELLED.to_hresult().into());
                }
                if hr != S_OK {
                    crate::log(&format!("show: OnFileOk answered {hr:?}, taken as accepted"));
                }
            }
            Ok(())
        }
    }

    impl IFileDialog_Impl for Dialog_Impl {
        fn SetFileTypes(&self, n: u32, specs: *const COMDLG_FILTERSPEC) -> Result<()> {
            if specs.is_null() {
                return Err(E_POINTER.into());
            }
            let specs = unsafe { std::slice::from_raw_parts(specs, n as usize) };
            self.st.borrow_mut().filters = specs.iter().map(|s| Filter { name: wstr(&s.pszName), spec: wstr(&s.pszSpec) }).collect();
            unsafe { self.real.SetFileTypes(specs) }
        }
        fn SetFileTypeIndex(&self, i: u32) -> Result<()> {
            self.st.borrow_mut().type_index = i;
            unsafe { self.real.SetFileTypeIndex(i) }
        }
        fn GetFileTypeIndex(&self) -> Result<u32> {
            if self.st.borrow().result.is_some() {
                return Ok(self.st.borrow().type_index);
            }
            unsafe { self.real.GetFileTypeIndex() }
        }
        fn Advise(&self, sink: Ref<IFileDialogEvents>) -> Result<u32> {
            let s = sink.ok()?;
            let cookie = unsafe { self.real.Advise(s)? };
            self.st.borrow_mut().sinks.push((cookie, s.clone()));
            Ok(cookie)
        }
        fn Unadvise(&self, cookie: u32) -> Result<()> {
            self.st.borrow_mut().sinks.retain(|(c, _)| *c != cookie);
            unsafe { self.real.Unadvise(cookie) }
        }
        fn SetOptions(&self, fos: FILEOPENDIALOGOPTIONS) -> Result<()> {
            self.st.borrow_mut().options = fos;
            unsafe { self.real.SetOptions(fos) }
        }
        fn GetOptions(&self) -> Result<FILEOPENDIALOGOPTIONS> {
            unsafe { self.real.GetOptions() }
        }
        fn SetDefaultFolder(&self, psi: Ref<IShellItem>) -> Result<()> {
            let i = psi.ok()?;
            self.st.borrow_mut().default_folder = item_path(i);
            unsafe { self.real.SetDefaultFolder(i) }
        }
        fn SetFolder(&self, psi: Ref<IShellItem>) -> Result<()> {
            let i = psi.ok()?;
            self.st.borrow_mut().folder = item_path(i);
            unsafe { self.real.SetFolder(i) }
        }
        fn GetFolder(&self) -> Result<IShellItem> {
            if let Some(first) = self.st.borrow().result.as_ref().and_then(|r| r.first().cloned()) {
                crate::log("GetFolder");
                if let Some(dir) = std::path::Path::new(&first).parent() {
                    return item_for(&dir.to_string_lossy());
                }
            }
            unsafe { self.real.GetFolder() }
        }
        fn GetCurrentSelection(&self) -> Result<IShellItem> {
            if let Some(first) = self.st.borrow().result.as_ref().and_then(|r| r.first().cloned()) {
                return item_for(&first);
            }
            unsafe { self.real.GetCurrentSelection() }
        }
        fn SetFileName(&self, name: &PCWSTR) -> Result<()> {
            self.st.borrow_mut().file_name = wstr(name);
            unsafe { self.real.SetFileName(*name) }
        }
        fn GetFileName(&self) -> Result<PWSTR> {
            if self.st.borrow().result.is_some() {
                crate::log("GetFileName");
                return co_string(&self.st.borrow().file_name);
            }
            unsafe { self.real.GetFileName() }
        }
        fn SetTitle(&self, title: &PCWSTR) -> Result<()> {
            self.st.borrow_mut().title = wstr(title);
            unsafe { self.real.SetTitle(*title) }
        }
        fn SetOkButtonLabel(&self, text: &PCWSTR) -> Result<()> {
            self.st.borrow_mut().ok_label = wstr(text);
            unsafe { self.real.SetOkButtonLabel(*text) }
        }
        fn SetFileNameLabel(&self, label: &PCWSTR) -> Result<()> {
            unsafe { self.real.SetFileNameLabel(*label) }
        }
        fn GetResult(&self) -> Result<IShellItem> {
            if let Some(first) = self.st.borrow().result.as_ref().and_then(|r| r.first().cloned()) {
                let r = item_for(&first);
                crate::log(&format!("GetResult -> {}", if r.is_ok() { "ok" } else { "FAILED" }));
                return r;
            }
            unsafe { self.real.GetResult() }
        }
        fn AddPlace(&self, psi: Ref<IShellItem>, fdap: FDAP) -> Result<()> {
            unsafe { self.real.AddPlace(psi.ok()?, fdap) }
        }
        fn SetDefaultExtension(&self, ext: &PCWSTR) -> Result<()> {
            self.st.borrow_mut().default_ext = wstr(ext);
            unsafe { self.real.SetDefaultExtension(*ext) }
        }
        fn Close(&self, hr: HRESULT) -> Result<()> {
            unsafe { self.real.Close(hr) }
        }
        fn SetClientGuid(&self, guid: *const GUID) -> Result<()> {
            if guid.is_null() {
                return Err(E_POINTER.into());
            }
            unsafe { self.real.SetClientGuid(&*guid) }
        }
        fn ClearClientData(&self) -> Result<()> {
            unsafe { self.real.ClearClientData() }
        }
        fn SetFilter(&self, filter: Ref<IShellItemFilter>) -> Result<()> {
            unsafe { self.real.SetFilter(filter.ok()?) }
        }
    }

    impl IFileOpenDialog_Impl for Dialog_Impl {
        fn GetResults(&self) -> Result<IShellItemArray> {
            if let Some(r) = self.st.borrow().result.as_ref() {
                let a = items_for(r);
                crate::log(&format!("GetResults -> {}", if a.is_ok() { "ok" } else { "FAILED" }));
                return a;
            }
            unsafe { self.real.cast::<IFileOpenDialog>()?.GetResults() }
        }
        fn GetSelectedItems(&self) -> Result<IShellItemArray> {
            if let Some(r) = self.st.borrow().result.as_ref() {
                return items_for(r);
            }
            unsafe { self.real.cast::<IFileOpenDialog>()?.GetSelectedItems() }
        }
    }

    impl IFileSaveDialog_Impl for Dialog_Impl {
        fn SetSaveAsItem(&self, psi: Ref<IShellItem>) -> Result<()> {
            let i = psi.ok()?;
            if let Some(p) = item_path(i) {
                let path = std::path::Path::new(&p);
                let mut st = self.st.borrow_mut();
                st.folder = path.parent().map(|d| d.to_string_lossy().into_owned());
                st.file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            }
            unsafe { self.real.cast::<IFileSaveDialog>()?.SetSaveAsItem(i) }
        }
        fn SetProperties(&self, store: Ref<IPropertyStore>) -> Result<()> {
            unsafe { self.real.cast::<IFileSaveDialog>()?.SetProperties(store.ok()?) }
        }
        fn SetCollectedProperties(&self, list: Ref<IPropertyDescriptionList>, append_default: WBOOL) -> Result<()> {
            unsafe { self.real.cast::<IFileSaveDialog>()?.SetCollectedProperties(list.ok()?, append_default.as_bool()) }
        }
        fn GetProperties(&self) -> Result<IPropertyStore> {
            unsafe { self.real.cast::<IFileSaveDialog>()?.GetProperties() }
        }
        fn ApplyProperties(&self, psi: Ref<IShellItem>, store: Ref<IPropertyStore>, hwnd: HWND, sink: Ref<IFileOperationProgressSink>) -> Result<()> {
            unsafe { self.real.cast::<IFileSaveDialog>()?.ApplyProperties(psi.ok()?, store.ok()?, hwnd, sink.as_ref()) }
        }
    }

    /// Every control the app adds marks the dialog as one only Windows' own can show.
    macro_rules! custom {
        ($self:ident, $mark:expr, $call:expr) => {{
            if $mark {
                $self.st.borrow_mut().customized = true;
            }
            let c: IFileDialogCustomize = $self.real.cast()?;
            #[allow(unused_unsafe)]
            unsafe {
                $call(c)
            }
        }};
    }

    impl IFileDialogCustomize_Impl for Dialog_Impl {
        fn EnableOpenDropDown(&self, id: u32) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.EnableOpenDropDown(id))
        }
        fn AddMenu(&self, id: u32, label: &PCWSTR) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddMenu(id, *label))
        }
        fn AddPushButton(&self, id: u32, label: &PCWSTR) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddPushButton(id, *label))
        }
        fn AddComboBox(&self, id: u32) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddComboBox(id))
        }
        fn AddRadioButtonList(&self, id: u32) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddRadioButtonList(id))
        }
        fn AddCheckButton(&self, id: u32, label: &PCWSTR, checked: WBOOL) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddCheckButton(id, *label, checked.as_bool()))
        }
        fn AddEditBox(&self, id: u32, text: &PCWSTR) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddEditBox(id, *text))
        }
        fn AddSeparator(&self, id: u32) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddSeparator(id))
        }
        fn AddText(&self, id: u32, text: &PCWSTR) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.AddText(id, *text))
        }
        fn SetControlLabel(&self, id: u32, label: &PCWSTR) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetControlLabel(id, *label))
        }
        fn GetControlState(&self, id: u32) -> Result<CDCONTROLSTATEF> {
            custom!(self, false, |c: IFileDialogCustomize| c.GetControlState(id))
        }
        fn SetControlState(&self, id: u32, state: CDCONTROLSTATEF) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetControlState(id, state))
        }
        fn GetEditBoxText(&self, id: u32) -> Result<*mut u16> {
            custom!(self, false, |c: IFileDialogCustomize| c.GetEditBoxText(id))
        }
        fn SetEditBoxText(&self, id: u32, text: &PCWSTR) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetEditBoxText(id, *text))
        }
        fn GetCheckButtonState(&self, id: u32) -> Result<WBOOL> {
            custom!(self, false, |c: IFileDialogCustomize| c.GetCheckButtonState(id))
        }
        fn SetCheckButtonState(&self, id: u32, checked: WBOOL) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetCheckButtonState(id, checked.as_bool()))
        }
        fn AddControlItem(&self, id: u32, item: u32, label: &PCWSTR) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.AddControlItem(id, item, *label))
        }
        fn RemoveControlItem(&self, id: u32, item: u32) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.RemoveControlItem(id, item))
        }
        fn RemoveAllControlItems(&self, id: u32) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.RemoveAllControlItems(id))
        }
        fn GetControlItemState(&self, id: u32, item: u32) -> Result<CDCONTROLSTATEF> {
            custom!(self, false, |c: IFileDialogCustomize| c.GetControlItemState(id, item))
        }
        fn SetControlItemState(&self, id: u32, item: u32, state: CDCONTROLSTATEF) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetControlItemState(id, item, state))
        }
        fn GetSelectedControlItem(&self, id: u32) -> Result<u32> {
            custom!(self, false, |c: IFileDialogCustomize| c.GetSelectedControlItem(id))
        }
        fn SetSelectedControlItem(&self, id: u32, item: u32) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetSelectedControlItem(id, item))
        }
        fn StartVisualGroup(&self, id: u32, label: &PCWSTR) -> Result<()> {
            custom!(self, true, |c: IFileDialogCustomize| c.StartVisualGroup(id, *label))
        }
        fn EndVisualGroup(&self) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.EndVisualGroup())
        }
        fn MakeProminent(&self, id: u32) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.MakeProminent(id))
        }
        fn SetControlItemText(&self, id: u32, item: u32, label: &PCWSTR) -> Result<()> {
            custom!(self, false, |c: IFileDialogCustomize| c.SetControlItemText(id, item, *label))
        }
    }

    #[implement(IClassFactory)]
    struct Factory {
        save: bool,
    }

    impl IClassFactory_Impl for Factory_Impl {
        fn CreateInstance(&self, outer: Ref<IUnknown>, iid: *const GUID, object: *mut *mut c_void) -> Result<()> {
            if object.is_null() || iid.is_null() {
                return Err(E_POINTER.into());
            }
            unsafe { *object = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            // PickerHost.exe shows the dialog for packaged apps' FileSavePicker / FileOpenPicker
            // (Snipping Tool's Save as). It drives Windows' dialog through a private interface
            // ({9EA5491C-89C8-4BEF-93D3-7F665FB82A33}) we can't pass on, and fails the pick
            // without it: it gets Windows' own dialog, untouched.
            if exe_name().eq_ignore_ascii_case("PickerHost.exe") {
                crate::log("create: PickerHost, Windows' dialog");
                let real = real_dialog(if self.save { &FileSaveDialog } else { &FileOpenDialog })?;
                return unsafe { real.query(iid, object).ok() };
            }
            let unk: IUnknown = Dialog::new(self.save)?.into();
            unsafe { unk.query(iid, object).ok() }
        }
        fn LockServer(&self, _lock: WBOOL) -> Result<()> {
            Ok(())
        }
    }

    /// COM entry point: our two classes.
    ///
    /// # Safety
    /// Called by COM with valid pointers.
    #[unsafe(no_mangle)]
    pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        if clsid.is_null() || iid.is_null() || out.is_null() {
            return E_POINTER;
        }
        unsafe { *out = std::ptr::null_mut() };
        let save = match unsafe { *clsid } {
            c if c == FileOpenDialog => false,
            c if c == FileSaveDialog => true,
            _ => return CLASS_E_CLASSNOTAVAILABLE,
        };
        let f: IClassFactory = Factory { save }.into();
        unsafe { f.query(iid, out) }
    }

    /// Kept loaded: the app may still hold objects, and loading is cheap.
    #[unsafe(no_mangle)]
    pub extern "system" fn DllCanUnloadNow() -> HRESULT {
        S_FALSE
    }

    #[allow(dead_code)]
    const _: (HRESULT, HRESULT) = (S_OK, E_NOINTERFACE);
}
