//! TS, PS and AVI readers on damaged files: cut off, bytes flipped, junk in the middle. They may
//! return an error, but must not panic, hang, or read samples outside the file.

mod common;

use std::io::Cursor;

use common::*;

/// Small deterministic generator (no rand crate here).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn exercise(bytes: Vec<u8>) {
    let mut c = Cursor::new(bytes);
    if let Ok(movie) = rustdecoder::container::read(&mut c) {
        let mut buf = Vec::new();
        for t in &movie.tracks {
            // every few samples must be readable or fail cleanly
            for i in (0..t.samples.len()).step_by(5) {
                let _ = t.read_sample(&mut c, i, &mut buf);
            }
        }
    }
}

fn clips() -> Vec<(&'static str, std::path::PathBuf)> {
    let h264 = ["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "keyint=30:bframes=3"];
    let mpeg2 = ["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-g", "12"];
    vec![
        ("ts", make_av("t_h264_aac.ts", &h264, &["-c:a", "aac", "-b:a", "96k"], &["-f", "mpegts"])),
        ("m2ts", make_av("t_h264_ac3.m2ts", &h264, &["-c:a", "ac3", "-b:a", "192k"], &["-f", "mpegts", "-mpegts_m2ts_mode", "1"])),
        ("avi", make_av("a_h264_mp3_b.avi", &h264, &["-c:a", "libmp3lame", "-b:a", "128k"], &["-f", "avi"])),
        ("vob", make_av("p_mpeg2_ac3.vob", &mpeg2, &["-c:a", "ac3", "-b:a", "192k"], &["-f", "vob"])),
    ]
}

#[test]
fn damaged_files_do_not_panic() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let mut rng = Lcg(12345);
    for (name, path) in clips() {
        let original = std::fs::read(&path).unwrap();
        let mut n = 0;
        // cut at random places
        for _ in 0..40 {
            let cut = rng.next() as usize % original.len();
            exercise(original[..cut].to_vec());
            n += 1;
        }
        // flip random bytes (a few, then many), mostly near the start where the headers are
        for round in 0..120 {
            let mut b = original.clone();
            let flips = if round < 60 { 3 } else { 200 };
            for _ in 0..flips {
                let span = if rng.next() % 2 == 0 { 8192.min(b.len()) } else { b.len() };
                let at = rng.next() as usize % span;
                b[at] = rng.next() as u8;
            }
            exercise(b);
            n += 1;
        }
        // zero a block in the middle
        let mut b = original.clone();
        let at = b.len() / 2;
        let end = at + 4000.min(b.len() - at);
        b[at..end].fill(0);
        exercise(b);
        n += 1;
        eprintln!("{name}: {n} damaged variants read without a panic");
    }
}
