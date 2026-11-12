//! Loads smoothyfiles_dialog.dll straight from the build folder (no registry) and shows its Open,
//! Save or folder dialog like an app would; prints what came back.
//!   probe open|save|folder <start folder>
use std::ffi::c_void;

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IClassFactory};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, w};

fn main() -> windows::core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("open");
    let start = args.get(2).cloned().unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dll = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("smoothyfiles_dialog.dll");
        let lib = LoadLibraryW(&HSTRING::from(dll.as_os_str()))?;
        let f: unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT =
            std::mem::transmute(GetProcAddress(lib, windows::core::s!("DllGetClassObject")).unwrap());
        let clsid = if mode == "save" { FileSaveDialog } else { FileOpenDialog };
        let mut cf: *mut c_void = std::ptr::null_mut();
        f(&clsid, &IClassFactory::IID, &mut cf).ok()?;
        let cf = IClassFactory::from_raw(cf);
        let d: IFileDialog = cf.CreateInstance::<Option<&IUnknown>, IFileDialog>(None)?;
        d.SetTitle(w!("Probe"))?;
        let specs = [
            COMDLG_FILTERSPEC { pszName: w!("Text"), pszSpec: w!("*.txt;*.md") },
            COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
        ];
        if mode != "folder" {
            d.SetFileTypes(&specs)?;
            d.SetFileTypeIndex(1)?;
        }
        let mut opts = d.GetOptions()?;
        if mode == "folder" {
            opts |= FOS_PICKFOLDERS;
        }
        if mode == "multi" {
            opts |= FOS_ALLOWMULTISELECT;
        }
        if mode == "save" {
            d.SetFileName(w!("notlar"))?;
            d.SetDefaultExtension(w!("txt"))?;
        }
        d.SetOptions(opts)?;
        let folder: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(start.as_str()), None)?;
        d.SetFolder(&folder)?;
        match d.Show(None) {
            Ok(()) if mode == "multi" => {
                // multi-select apps read the array
                let arr = d.cast::<IFileOpenDialog>()?.GetResults()?;
                for i in 0..arr.GetCount()? {
                    let p = arr.GetItemAt(i)?.GetDisplayName(SIGDN_FILESYSPATH)?;
                    println!("RESULT {}", p.to_string().unwrap_or_default());
                    CoTaskMemFree(Some(p.0 as *const c_void));
                }
            }
            Ok(()) => {
                let item = d.GetResult()?;
                let p = item.GetDisplayName(SIGDN_FILESYSPATH)?;
                println!("RESULT {} (type {})", p.to_string().unwrap_or_default(), d.GetFileTypeIndex().unwrap_or(0));
                CoTaskMemFree(Some(p.0 as *const c_void));
            }
            Err(e) => println!("CANCELLED {:?}", e.code()),
        }
    }
    Ok(())
}
