use rf_player::{Frame, to_bgra};

fn px(b: &[u8], w: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let i = ((y * w + x) * 4) as usize;
    (b[i + 2], b[i + 1], b[i], b[i + 3])
}

#[test]
fn gray_limited_bt709() {
    let b = to_bgra(&Frame::yuv420_solid(16, 16, 126, 128, 128), 8, 8);
    assert_eq!(b.len(), 8 * 8 * 4);
    let (r, g, bl, a) = px(&b, 8, 3, 3);
    assert!((r as i32 - 128).abs() <= 1 && r == g && g == bl && a == 255, "{r} {g} {bl}");
}

#[test]
fn red_limited_bt709() {
    let b = to_bgra(&Frame::yuv420_solid(16, 16, 63, 102, 240), 16, 16);
    let (r, g, bl, _) = px(&b, 16, 5, 5);
    assert!(r >= 250 && g <= 6 && bl <= 6, "{r} {g} {bl}");
}

#[test]
fn ten_bit_white() {
    let f = Frame::yuv420_solid_16(16, 16, 10, 940, 512, 512);
    let (r, g, b, _) = px(&to_bgra(&f, 4, 4), 4, 1, 1);
    assert!(r >= 253 && g >= 253 && b >= 253, "{r} {g} {b}");
}

#[test]
fn decoded_frame_scales() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/h264.mp4");
    let m = rf_player::open(&path).unwrap();
    let f = m.frame_at(0.2).unwrap();
    let b = to_bgra(&f, 80, 45);
    assert_eq!(b.len(), 80 * 45 * 4);
    let first = px(&b, 80, 0, 0);
    assert!((0..45).step_by(5).any(|y| (0..80).step_by(5).any(|x| px(&b, 80, x, y) != first)));
}
