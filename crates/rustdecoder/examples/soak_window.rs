//! Opens and closes a playing video window many times (hidden, muted) and prints the process'
//! private memory and handles, to catch what a closed window leaves behind.
//! `cargo run --release -p rustdecoder --example soak_window -- <clip> <rounds>`

use std::time::Duration;

use rustdecoder::window::{Placement, VideoWindow, WindowOptions};

fn private_and_handles() -> (f64, u32) {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    unsafe {
        let mut m = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb);
        let mut h = 0u32;
        let _ = GetProcessHandleCount(GetCurrentProcess(), &mut h);
        (m.PrivateUsage as f64 / 1048576.0, h)
    }
}

fn main() {
    let mut a = std::env::args().skip(1);
    let clip = std::path::PathBuf::from(a.next().expect("clip"));
    let rounds: usize = a.next().map_or(20, |s| s.parse().unwrap());
    if std::env::var_os("SOAK_HWDEC_ONLY").is_some() {
        let (profile, format) = rustdecoder::d3d11::h264_profile();
        let g = rustdecoder::d3d11::Gpu::open(&profile, format).expect("gpu");
        for i in 0..rounds {
            let d = rustdecoder::d3d11::HwDecoder::new(&g, &profile, format, 640, 368, 8).expect("decoder");
            drop(d);
            unsafe {
                g.context.Flush();
            }
            let (mb, h) = private_and_handles();
            if i == 0 || (i + 1) % 5 == 0 {
                println!("after hw decoder {:2}: {mb:6.1} MB private, {h} handles", i + 1);
            }
        }
        return;
    }
    if std::env::var_os("SOAK_DEVICE_ONLY").is_some() {
        for i in 0..rounds {
            let (profile, format) = rustdecoder::d3d11::h264_profile();
            let g = rustdecoder::d3d11::Gpu::open(&profile, format).expect("gpu");
            drop(g);
            let (mb, h) = private_and_handles();
            if i == 0 || (i + 1) % 5 == 0 {
                println!("after device {:2}: {mb:6.1} MB private, {h} handles", i + 1);
            }
        }
        return;
    }
    if std::env::var_os("SOAK_DECODER_ONLY").is_some() {
        for i in 0..rounds {
            let mut d = rustdecoder::video::VideoDecoder::open(&clip).expect("open");
            let frames: usize = std::env::var("SOAK_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
            for _ in 0..frames {
                let _ = d.next_frame();
            }
            drop(d);
            let (mb, h) = private_and_handles();
            if i == 0 || (i + 1) % 5 == 0 {
                println!("after decoder {:2}: {mb:6.1} MB private, {h} handles", i + 1);
            }
        }
        return;
    }
    for i in 0..rounds {
        let audio = std::env::var_os("SOAK_NO_AUDIO").is_none();
        let opts = WindowOptions { muted: true, visible: false, audio, ..Default::default() };
        let w = VideoWindow::open(&clip, Placement::TopLevel { title: "soak".into(), width: 640, height: 360 }, opts).expect("open");
        std::thread::sleep(Duration::from_millis(1200));
        w.seek_fraction(0.5);
        std::thread::sleep(Duration::from_millis(400));
        drop(w);
        std::thread::sleep(Duration::from_millis(300));
        let (mb, h) = private_and_handles();
        if i == 0 || (i + 1) % 5 == 0 {
            println!("after window {:2}: {mb:6.1} MB private, {h} handles", i + 1);
        }
    }
}
