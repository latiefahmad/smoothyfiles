//! Lists the "Open with" handlers Windows offers for an extension: `assoc_handlers .7z`.
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::UI::Shell::{ASSOC_FILTER_NONE, ASSOC_FILTER_RECOMMENDED, IAssocHandler, SHAssocEnumHandlers};
use windows::core::HSTRING;

fn main() -> windows::core::Result<()> {
    let ext = std::env::args().nth(1).unwrap_or_else(|| ".7z".into());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        for (label, filter) in [("önerilen", ASSOC_FILTER_RECOMMENDED), ("hepsi", ASSOC_FILTER_NONE)] {
            println!("--- {label}");
            let en = SHAssocEnumHandlers(&HSTRING::from(ext.as_str()), filter)?;
            loop {
                let mut h: [Option<IAssocHandler>; 1] = [None];
                let mut n = 0u32;
                if en.Next(&mut h, Some(&mut n)).is_err() || n == 0 {
                    break;
                }
                let h = h[0].take().unwrap();
                let name = h.GetName().map(|p| { let s = p.to_string().unwrap_or_default(); CoTaskMemFree(Some(p.0 as _)); s }).unwrap_or_default();
                let ui = h.GetUIName().map(|p| { let s = p.to_string().unwrap_or_default(); CoTaskMemFree(Some(p.0 as _)); s }).unwrap_or_default();
                println!("{ui}  |  {name}");
            }
        }
    }
    Ok(())
}
