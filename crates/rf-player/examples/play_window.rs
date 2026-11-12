//! Deneme penceresi: `cargo run --release -p rf-player --example play_window -- <video> [--sound]`
//! Videoyu küçük bir pencerede oynatır, bitince kapanır. Varsayılan sessizdir (NullSink).

use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, MSG, PM_REMOVE,
    PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, ShowWindow, TranslateMessage, WM_DESTROY, WM_QUIT, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::w;

/// Pencerenin kendi görüntüsünü (üstünde başka pencere olsa da) 24 bit BMP olarak kaydeder (deneme için).
fn shot(hwnd: HWND, path: &str) {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        // PW_RENDERFULLCONTENT: DirectX içeriği de, pencere arkada kalsa bile.
        let _ = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(2));
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER { biSize: 40, biWidth: w, biHeight: h, biPlanes: 1, biBitCount: 24, ..Default::default() };
        let stride = ((w * 3 + 3) & !3) as usize;
        let mut px = vec![0u8; stride * h as usize];
        GetDIBits(mem, bmp, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let mut f = Vec::new();
        let size = 54 + px.len() as u32;
        f.extend_from_slice(b"BM");
        f.extend_from_slice(&size.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&54u32.to_le_bytes());
        f.extend_from_slice(&40u32.to_le_bytes());
        f.extend_from_slice(&w.to_le_bytes());
        f.extend_from_slice(&h.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&24u16.to_le_bytes());
        f.extend_from_slice(&[0u8; 24]);
        f.extend_from_slice(&px);
        let _ = std::fs::write(path, f);
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("video yolu");
    let sound = std::env::args().any(|a| a == "--sound");
    let shot_path = std::env::args().skip_while(|a| a != "--shot").nth(1);
    let started = std::time::Instant::now();
    let mut shot_done = false;
    let media = rf_player::open(std::path::Path::new(&path))?;
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: inst.into(),
            lpszClassName: w!("rf_player_trial"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("rf_player_trial"),
            w!("rf-player deneme"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            660,
            400,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut presenter = rf_player::Presenter::new(hwnd.0 as isize)?;
        let sink: Box<dyn rf_player::AudioSink> = if sound { rf_player::wasapi_sink()? } else { Box::new(rf_player::NullSink::new(48000, 2)) };
        let mut player = rf_player::Player::new(media, sink);
        player.play();
        let mut shown = f64::NAN;
        let mut frames = 0u32;
        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    println!("{frames} kare gösterildi");
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            presenter.resize((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32);
            if let Some(f) = player.current_frame() {
                if f.pts != shown {
                    shown = f.pts;
                    frames += 1;
                    presenter.present(&f);
                }
            }
            if let Some(p) = shot_path.as_deref().filter(|_| !shot_done && started.elapsed() > Duration::from_millis(1500)) {
                shot(hwnd, p);
                shot_done = true;
            }
            if player.state().ended {
                println!("{frames} kare gösterildi, {:?}", player.state());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}
