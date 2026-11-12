//! "Open the folder and select this file" from other apps while smoothyfiles is the default file
//! manager: `SHOpenFolderAndSelectItems`, what Telegram's and the browsers' "Show in folder" call.
//!
//! Windows opens the folder through the folder verb we took over, so the file to select never
//! reaches our command line. It then looks for an Explorer window showing that folder (the
//! shell's window list, `IShellWindows`) and asks that window to select the item. We answer as
//! one: an invisible window registered with the shell for the folder, whose
//! `IShellView::SelectItem` hands us the item.
//!
//! The shell's side, as Explorer++ and Files found it (neither documented by Microsoft): it
//! finds the window with `FindWindowSW` (type `SWC_BROWSER` only), asks the registered object
//! for `IWebBrowserApp`, takes `get_Document`, asks that for `IServiceProvider`, queries the
//! view (`IShellView`) and calls `SelectItem` with the item's relative ID list. A pending
//! registration on the same thread must come first, the third `RegisterPending` argument is an
//! empty VARIANT, and the location is the folder's ID list as a byte-array VARIANT.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL, HWND, LPARAM, RECT, S_FALSE, SHANDLE_PTR, VARIANT_BOOL, VARIANT_FALSE, VARIANT_TRUE, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, IDispatch, IDispatch_Impl, IServiceProvider,
    IServiceProvider_Impl, ITypeInfo,
};
use windows::Win32::System::Ole::IOleWindow_Impl;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{InitVariantFromBuffer, VARIANT};
use windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    FOLDERSETTINGS, ILCombine, ILFree, ILGetSize, IShellBrowser, IShellView, IShellView_Impl, IShellWindows, IWebBrowser_Impl, IWebBrowserApp, IWebBrowserApp_Impl, SHGetNameFromIDList,
    SHParseDisplayName, SIGDN_FILESYSPATH, SVSI_EDIT, SVSI_ENSUREVISIBLE, SVSI_FOCUSED, SVSI_SELECT, SWC_BROWSER, ShellWindows, _SVGIO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG, PostMessageW, SetTimer, TranslateMessage, WINDOW_STYLE, WM_APP, WM_TIMER, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, BSTR, GUID, HRESULT, HSTRING, IUnknown, IUnknownImpl, Interface, PCWSTR, Ref, Result, implement, w};

#[implement(IWebBrowserApp, IServiceProvider, IShellView)]
struct ShellWindow {
    hwnd: HWND,
    /// The folder's absolute ID list (its bytes: the object may outlive the caller's copy).
    folder: Vec<u8>,
    name: String,
    /// Set by SelectItem, which COM may call on another thread than the waiting one.
    picked: Arc<Mutex<Option<PathBuf>>>,
}

fn not_impl<T>() -> Result<T> {
    Err(E_NOTIMPL.into())
}

impl IDispatch_Impl for ShellWindow_Impl {
    // the shell asks for IDispatch but never calls it
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Ok(0)
    }
    fn GetTypeInfo(&self, _: u32, _: u32) -> Result<ITypeInfo> {
        not_impl()
    }
    fn GetIDsOfNames(&self, _: *const GUID, _: *const PCWSTR, _: u32, _: u32, _: *mut i32) -> Result<()> {
        not_impl()
    }
    fn Invoke(&self, _: i32, _: *const GUID, _: u32, _: DISPATCH_FLAGS, _: *const DISPPARAMS, _: *mut VARIANT, _: *mut EXCEPINFO, _: *mut u32) -> Result<()> {
        not_impl()
    }
}

impl IWebBrowser_Impl for ShellWindow_Impl {
    fn GoBack(&self) -> Result<()> {
        not_impl()
    }
    fn GoForward(&self) -> Result<()> {
        not_impl()
    }
    fn GoHome(&self) -> Result<()> {
        not_impl()
    }
    fn GoSearch(&self) -> Result<()> {
        not_impl()
    }
    fn Navigate(&self, _: &BSTR, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> {
        not_impl()
    }
    fn Refresh(&self) -> Result<()> {
        Ok(())
    }
    fn Refresh2(&self, _: *const VARIANT) -> Result<()> {
        Ok(())
    }
    fn Stop(&self) -> Result<()> {
        Ok(())
    }
    fn Application(&self) -> Result<IDispatch> {
        self.to_interface::<IWebBrowserApp>().cast()
    }
    fn Parent(&self) -> Result<IDispatch> {
        self.to_interface::<IWebBrowserApp>().cast()
    }
    fn Container(&self) -> Result<IDispatch> {
        not_impl()
    }
    /// The "document": this same object, which answers IServiceProvider.
    fn Document(&self) -> Result<IDispatch> {
        self.to_interface::<IWebBrowserApp>().cast()
    }
    fn TopLevelContainer(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_TRUE)
    }
    fn Type(&self) -> Result<BSTR> {
        Ok(BSTR::from("File Folder"))
    }
    fn Left(&self) -> Result<i32> {
        Ok(0)
    }
    fn SetLeft(&self, _: i32) -> Result<()> {
        Ok(())
    }
    fn Top(&self) -> Result<i32> {
        Ok(0)
    }
    fn SetTop(&self, _: i32) -> Result<()> {
        Ok(())
    }
    fn Width(&self) -> Result<i32> {
        Ok(0)
    }
    fn SetWidth(&self, _: i32) -> Result<()> {
        Ok(())
    }
    fn Height(&self) -> Result<i32> {
        Ok(0)
    }
    fn SetHeight(&self, _: i32) -> Result<()> {
        Ok(())
    }
    fn LocationName(&self) -> Result<BSTR> {
        Ok(BSTR::from(self.name.as_str()))
    }
    fn LocationURL(&self) -> Result<BSTR> {
        not_impl()
    }
    fn Busy(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
}

impl IWebBrowserApp_Impl for ShellWindow_Impl {
    fn Quit(&self) -> Result<()> {
        Ok(())
    }
    fn ClientToWindow(&self, _: *mut i32, _: *mut i32) -> Result<()> {
        not_impl()
    }
    fn PutProperty(&self, _: &BSTR, _: &VARIANT) -> Result<()> {
        not_impl()
    }
    fn GetProperty(&self, _: &BSTR) -> Result<VARIANT> {
        not_impl()
    }
    fn Name(&self) -> Result<BSTR> {
        Ok(BSTR::from("smoothyfiles"))
    }
    fn HWND(&self) -> Result<SHANDLE_PTR> {
        Ok(SHANDLE_PTR(self.hwnd.0 as isize))
    }
    fn FullName(&self) -> Result<BSTR> {
        not_impl()
    }
    fn Path(&self) -> Result<BSTR> {
        not_impl()
    }
    fn Visible(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_TRUE)
    }
    fn SetVisible(&self, _: VARIANT_BOOL) -> Result<()> {
        Ok(())
    }
    fn StatusBar(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetStatusBar(&self, _: VARIANT_BOOL) -> Result<()> {
        Ok(())
    }
    fn StatusText(&self) -> Result<BSTR> {
        Ok(BSTR::new())
    }
    fn SetStatusText(&self, _: &BSTR) -> Result<()> {
        Ok(())
    }
    fn ToolBar(&self) -> Result<i32> {
        Ok(0)
    }
    fn SetToolBar(&self, _: i32) -> Result<()> {
        Ok(())
    }
    fn MenuBar(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetMenuBar(&self, _: VARIANT_BOOL) -> Result<()> {
        Ok(())
    }
    fn FullScreen(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetFullScreen(&self, _: VARIANT_BOOL) -> Result<()> {
        Ok(())
    }
}

impl IServiceProvider_Impl for ShellWindow_Impl {
    /// Whatever service is named, the view (IShellView / IFolderView are asked for).
    fn QueryService(&self, _: *const GUID, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        let unk: IUnknown = self.to_interface::<IShellView>().cast()?;
        unsafe { unk.query(riid, ppv).ok() }
    }
}

impl IOleWindow_Impl for ShellWindow_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        Ok(self.hwnd)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> {
        not_impl()
    }
}

impl IShellView_Impl for ShellWindow_Impl {
    fn TranslateAccelerator(&self, _: *const MSG) -> HRESULT {
        S_FALSE
    }
    fn EnableModeless(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
    fn UIActivate(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn Refresh(&self) -> Result<()> {
        Ok(())
    }
    fn CreateViewWindow(&self, _: Ref<IShellView>, _: *const FOLDERSETTINGS, _: Ref<IShellBrowser>, _: *const RECT) -> Result<HWND> {
        not_impl()
    }
    fn DestroyViewWindow(&self) -> Result<()> {
        Ok(())
    }
    fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> {
        Ok(FOLDERSETTINGS::default())
    }
    fn AddPropertySheetPages(&self, _: u32, _: LPFNSVADDPROPSHEETPAGE, _: LPARAM) -> Result<()> {
        not_impl()
    }
    fn SaveViewState(&self) -> Result<()> {
        Ok(())
    }
    /// The item to show selected: its full path goes to the waiting caller.
    fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> Result<()> {
        let wanted = (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0 | SVSI_EDIT.0) as u32;
        if item.is_null() || flags & wanted == 0 {
            return Ok(());
        }
        unsafe {
            let full = ILCombine(Some(self.folder.as_ptr().cast()), Some(item));
            if full.is_null() {
                return Ok(());
            }
            let name = SHGetNameFromIDList(full, SIGDN_FILESYSPATH);
            ILFree(Some(full));
            if let Ok(name) = name {
                let path = name.to_string().ok().map(PathBuf::from);
                CoTaskMemFree(Some(name.0 as *const _));
                if let Some(path) = path {
                    *self.picked.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
                    // ends the wait below, from whichever thread COM called us on
                    let _ = PostMessageW(Some(self.hwnd), WM_PICKED, WPARAM(0), LPARAM(0));
                }
            }
        }
        Ok(())
    }
    fn GetItemObject(&self, _: &_SVGIO, _: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        if !ppv.is_null() {
            unsafe { *ppv = std::ptr::null_mut() };
        }
        Err(E_NOINTERFACE.into())
    }
}

/// Registers a shell window for `folder` on this thread and waits up to `wait` for the shell to
/// ask for an item to be selected there: that item's path. Blocking (a message loop); call it
/// on a thread of its own, right when smoothyfiles is started on a folder.
pub fn wait_for_select(folder: &Path, wait: Duration) -> Option<PathBuf> {
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = register_and_wait(folder, wait);
        if init {
            CoUninitialize();
        }
        r
    }
}

unsafe fn register_and_wait(folder: &Path, wait: Duration) -> Option<PathBuf> {
    unsafe {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&HSTRING::from(folder.as_os_str()), None, &mut pidl, 0, None).ok()?;
        if pidl.is_null() {
            return None;
        }
        let size = ILGetSize(Some(pidl));
        let bytes = std::slice::from_raw_parts(pidl as *const u8, size as usize).to_vec();
        let location = InitVariantFromBuffer(pidl as *const _, size);
        CoTaskMemFree(Some(pidl as *const _));
        let location = location.ok()?;

        // never shown, never activated (as Files' launcher)
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("STATIC"), w!("smoothyfiles shell window"), WINDOW_STYLE(0), 0, 0, 0, 0, None, None, None, None).ok()?;
        let picked = Arc::new(Mutex::new(None));
        let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| folder.to_string_lossy().into_owned());
        let app: IWebBrowserApp = ShellWindow { hwnd, folder: bytes, name, picked: picked.clone() }.into();
        let registered = (|| -> Result<(IShellWindows, i32)> {
            let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
            let empty = VARIANT::default();
            windows.RegisterPending(GetCurrentThreadId() as i32, &location, &empty, SWC_BROWSER)?;
            let dispatch: IDispatch = app.cast()?;
            let cookie = windows.Register(&dispatch, hwnd.0 as isize as i32, SWC_BROWSER)?;
            windows.OnNavigate(cookie, &location)?;
            Ok((windows, cookie))
        })();
        let registered = match registered {
            Ok(r) => Some(r),
            Err(e) => {
                log::warn!("shell window for {}: {e}", folder.display());
                None
            }
        };
        if registered.is_some() {
            SetTimer(Some(hwnd), 1, wait.as_millis().min(u32::MAX as u128) as u32, None);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                if msg.hwnd == hwnd && (msg.message == WM_TIMER || msg.message == WM_PICKED) {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = KillTimer(Some(hwnd), 1);
        }
        if let Some((windows, cookie)) = registered {
            let _ = windows.Revoke(cookie);
        }
        let _ = DestroyWindow(hwnd);
        drop(app);
        picked.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Posted to the window when an item was picked.
const WM_PICKED: u32 = WM_APP + 1;

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::IServiceProvider;
    use windows::Win32::UI::Shell::{IFolderView, SWFO_NEEDDISPATCH};

    /// Plays the shell's part of "open and select": finds our registered window for a folder the
    /// way SHOpenFolderAndSelectItems does and asks it to select a file; the waiting side gets
    /// the file's path.
    #[test]
    fn the_shell_finds_the_window_and_selection_arrives() {
        let dir = std::env::temp_dir().join(format!("rf-shellwin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rapor ü.txt");
        std::fs::write(&file, b"x").unwrap();
        let d = dir.clone();
        let t0 = std::time::Instant::now();
        let waiter = std::thread::spawn(move || (wait_for_select(&d, Duration::from_secs(10)), t0.elapsed()));
        // the shell's side, on another thread (its own apartment)
        let shell = std::thread::spawn({
            let dir = dir.clone();
            move || unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).unwrap();
                let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
                SHParseDisplayName(&HSTRING::from(dir.as_os_str()), None, &mut pidl, 0, None).unwrap();
                let loc = InitVariantFromBuffer(pidl as *const _, ILGetSize(Some(pidl))).unwrap();
                let empty = VARIANT::default();
                let mut found = None;
                for _ in 0..100 {
                    let mut hwnd = 0i32;
                    if let Ok(disp) = windows.FindWindowSW(&loc, &empty, SWC_BROWSER, &mut hwnd, SWFO_NEEDDISPATCH) {
                        found = Some(disp);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let disp = found.expect("registered window found");
                let app: IWebBrowserApp = disp.cast().unwrap();
                let doc = app.Document().unwrap();
                let sp: IServiceProvider = doc.cast().unwrap();
                let view: IShellView = sp.QueryService::<IShellView>(&IFolderView::IID).unwrap();
                // the file's ID list relative to the folder: its last item
                let mut fpidl: *mut ITEMIDLIST = std::ptr::null_mut();
                SHParseDisplayName(&HSTRING::from(dir.join("rapor ü.txt").as_os_str()), None, &mut fpidl, 0, None).unwrap();
                let child = windows::Win32::UI::Shell::ILFindLastID(fpidl);
                view.SelectItem(child, (SVSI_SELECT.0 | SVSI_ENSUREVISIBLE.0 | SVSI_FOCUSED.0) as u32).unwrap();
                CoTaskMemFree(Some(pidl as *const _));
                CoTaskMemFree(Some(fpidl as *const _));
                CoUninitialize();
            }
        });
        shell.join().unwrap();
        let (got, took) = waiter.join().unwrap();
        eprintln!("selection after {took:?}");
        // the wait ends with the selection, not at its limit
        assert!(took < Duration::from_secs(5), "{took:?}");
        let _ = std::fs::remove_dir_all(&dir);
        // the shell hands back the long path; temp_dir can be a short (8.3) path on some
        // runners, so both sides go through canonicalize (the long form) before comparing
        let norm = |p: &std::path::Path| {
            std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase()
        };
        assert_eq!(got.as_deref().map(norm).as_deref(), Some(norm(&file).as_str()));
    }
}
