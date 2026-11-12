use rf_player::{Frame, render_offscreen};

fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> (u8, u8, u8) {
    let i = ((y * w + x) * 4) as usize;
    (rgba[i], rgba[i + 1], rgba[i + 2])
}

#[test]
fn converts_bt709_limited_gray() {
    let f = Frame::yuv420_solid(16, 16, 126, 128, 128);
    let rgba = render_offscreen(&f, 16, 16).unwrap();
    let (r, g, b) = px(&rgba, 16, 0, 0);
    assert!((r as i32 - 128).abs() <= 2 && r == g && g == b, "{r} {g} {b}");
}

#[test]
fn converts_bt709_limited_red() {
    // BT.709 sınırlı aralıkta saf kırmızı: Y=63, Cb=102, Cr=240.
    let f = Frame::yuv420_solid(16, 16, 63, 102, 240);
    let rgba = render_offscreen(&f, 16, 16).unwrap();
    let (r, g, b) = px(&rgba, 16, 8, 8);
    assert!(r >= 250 && g <= 6 && b <= 6, "{r} {g} {b}");
}

#[test]
fn letterboxes_with_black() {
    // 16x16 kare 32x16 hedefe: solda ve sağda 8 piksel siyah kenar.
    let f = Frame::yuv420_solid(16, 16, 235, 128, 128);
    let rgba = render_offscreen(&f, 32, 16).unwrap();
    assert_eq!(px(&rgba, 32, 1, 8), (0, 0, 0));
    assert_eq!(px(&rgba, 32, 30, 8), (0, 0, 0));
    let (r, g, b) = px(&rgba, 32, 16, 8);
    assert!(r >= 250 && g >= 250 && b >= 250);
}

#[test]
fn decoded_frame_renders() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/h264.mp4");
    let m = rf_player::open(&path).unwrap();
    let f = m.frame_at(0.2).unwrap();
    let rgba = render_offscreen(&f, 160, 90).unwrap();
    assert_eq!(rgba.len(), 160 * 90 * 4);
    // testsrc2 renkli: tek renkli olmamalı.
    let first = px(&rgba, 160, 0, 0);
    assert!((0..90).step_by(10).any(|y| (0..160).step_by(10).any(|x| px(&rgba, 160, x, y) != first)));
}
