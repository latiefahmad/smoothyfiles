//! Decodes the first frame of a clip on the GPU and writes it as a PNG:
//! `cargo run --release -p rustdecoder --example first_frame -- <clip> <out.png> [frame index]`
//! The conversion to RGB is a plain CPU one (BT.709, limited range); the shader path comes
//! with the player.

#[cfg(windows)]
fn main() {
    use std::io::Write;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: first_frame <clip> <out.png> [frame index]");
        std::process::exit(2);
    }
    let index: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut dec = rustdecoder::VideoDecoder::open(std::path::Path::new(&args[0])).expect("open");
    let mut frame = None;
    for _ in 0..=index {
        frame = dec.next_frame().expect("decode");
        if frame.is_none() {
            break;
        }
    }
    let frame = frame.expect("the clip has fewer frames");
    let p = dec.read_back(&frame).expect("read back");
    println!("{}x{} {}-bit, POC {}, adapter {}", p.width, p.height, p.bit_depth, frame.poc, dec.adapter());

    let (w, h) = (p.width as usize, p.height as usize);
    println!("mean luma {:.1}", p.y.iter().map(|&v| v as f64).sum::<f64>() / p.y.len() as f64);
    let shift = p.bit_depth as u32 - 8;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let yy = (p.y[y * w + x] >> shift) as f32;
            let c = (y / 2) * w + (x / 2) * 2;
            let (u, v) = ((p.uv[c] >> shift) as f32 - 128.0, (p.uv[c + 1] >> shift) as f32 - 128.0);
            let l = (yy - 16.0) * (255.0 / 219.0);
            let (u, v) = (u * (255.0 / 224.0), v * (255.0 / 224.0));
            let r = l + 1.5748 * v;
            let g = l - 0.1873 * u - 0.4681 * v;
            let b = l + 1.8556 * u;
            rgb.extend_from_slice(&[r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8]);
        }
    }
    std::fs::write(&args[1], png(w as u32, h as u32, &rgb)).expect("write");
    println!("wrote {}", args[1]);
    let _ = std::io::stdout().flush();
}

/// A minimal PNG: 8-bit RGB, no filtering.
#[cfg(windows)]
fn png(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut raw = Vec::with_capacity(rgb.len() + h as usize);
    for row in rgb.chunks(w as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(&raw).unwrap();
    let idat = z.finish().unwrap();
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut c = crc32fast::Hasher::new();
        c.update(kind);
        c.update(data);
        out.extend_from_slice(&c.finalize().to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &idat);
    chunk(b"IEND", &[]);
    out
}

#[cfg(not(windows))]
fn main() {}
