use std::path::PathBuf;
use std::time::{Duration, Instant};

use rf_player::open;

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

#[test]
fn opens_h264_mp4() {
    let m = open(&data("h264.mp4")).unwrap();
    let v = m.info().video.clone().unwrap();
    assert_eq!((v.width, v.height), (160, 90));
    assert!(m.info().audio_streams.is_empty());
}

#[test]
fn decodes_frames_of_each_codec() {
    for f in ["h264.mp4", "hevc.mkv", "prores.mov"] {
        let m = open(&data(f)).unwrap();
        let fr = m.frame_at(0.5).unwrap();
        assert_eq!((fr.width, fr.height), (160, 90), "{f}");
    }
}

#[test]
fn audio_only_file() {
    let m = open(&data("silent.mp3")).unwrap();
    assert!(m.info().video.is_none());
    let a = m.audio(0, 4800, 48000).unwrap();
    assert!(!a.is_empty());
    assert!(a.iter().all(|s| s.abs() < 1e-3));
}

#[test]
fn can_open_by_extension() {
    assert!(rf_player::can_open(&data("h264.mp4")));
    assert!(rf_player::can_open(&data("silent.mp3")));
    assert!(!rf_player::can_open(std::path::Path::new("a.txt")));
}

/// 4 GB'lık seyrek bir dosyanın başında h264.mp4 var; açma 1 s içinde döner ve bellek 200 MB'ı aşmaz.
#[test]
fn huge_file_is_not_read_whole() {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let path = std::env::temp_dir().join(format!("rf-player-huge-{}.mp4", std::process::id()));
    {
        let mut f = std::fs::File::create(&path).unwrap();
        unsafe {
            let mut ret = 0u32;
            DeviceIoControl(HANDLE(f.as_raw_handle()), FSCTL_SET_SPARSE, None, 0, None, 0, Some(&mut ret), None).unwrap();
        }
        let head = std::fs::read(data("h264.mp4")).unwrap();
        f.write_all(&head).unwrap();
        let total: u64 = 4 << 30;
        let rest = total - head.len() as u64;
        // Geri kalanı tek bir 64 bit "free" kutusu: ayrıştırıcı onu atlar.
        f.write_all(&1u32.to_be_bytes()).unwrap();
        f.write_all(b"free").unwrap();
        f.write_all(&rest.to_be_bytes()).unwrap();
        f.set_len(total).unwrap();
    }
    let t = Instant::now();
    let opened = open(&path);
    let took = t.elapsed();
    let mut pmc = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb).unwrap() };
    let ok = opened.as_ref().map(|m| m.info().video.is_some()).map_err(|e| e.to_string());
    drop(opened);
    let _ = std::fs::remove_file(&path);
    assert_eq!(ok, Ok(true));
    assert!(took < Duration::from_secs(1), "open took {took:?}");
    assert!(pmc.PeakPagefileUsage < 200 << 20, "peak private bytes {} MB", pmc.PeakPagefileUsage >> 20);
}

/// Karenin zamanı, istenen anı kapsayan karenin başlangıcıdır (25 kare/s: 0,5 s → 0,48 s).
#[test]
fn frame_pts_is_frame_start() {
    let m = open(&data("h264.mp4")).unwrap();
    assert!((m.frame_at(0.5).unwrap().pts - 0.48).abs() < 1e-6);
    assert!((m.frame_at(0.0).unwrap().pts).abs() < 1e-9);
    assert!((m.frame_at(0.9).unwrap().pts - 0.88).abs() < 1e-6);
}
