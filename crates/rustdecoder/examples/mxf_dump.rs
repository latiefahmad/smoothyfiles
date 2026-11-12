//! Prints what the MXF reader found: `mxf_dump <file> [samples to list per track]`.
fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("usage: mxf_dump <file> [n]");
    let n: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut f = std::fs::File::open(&path).unwrap();
    let m = match rustdecoder::mxf::parse(&mut f) {
        Ok(m) => m,
        Err(e) => return println!("failed: {e}"),
    };
    let hex = |u: &[u8]| u.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
    println!("run-in {} pattern {:?} incomplete {} index segments {} entries (total, temporal, random access) {:?} ebc {}", m.run_in, m.pattern, m.incomplete, m.index_segments, m.index_entries, m.edit_unit_byte_count);
    for p in &m.partitions {
        println!("  {:?} at {} closed {} complete {} hdr {} idx {} bodySID {} idxSID {} bodyoff {}", p.kind, p.pos, p.closed, p.complete, p.header_byte_count, p.index_byte_count, p.body_sid, p.index_sid, p.body_offset);
    }
    for c in &m.essence_containers {
        println!("  essence container {}", hex(c));
    }
    for (t, tr) in m.tracks.iter().zip(&m.movie.tracks) {
        println!("track {} num {:08x} {:?} edit rate {:?} dur {} wrap {:?} codec {:?} fourcc {}", t.track_id, t.track_number, t.kind, t.edit_rate, t.duration, t.wrapping, t.codec, String::from_utf8_lossy(&tr.fourcc));
        println!("  container {}", hex(&t.essence_container));
        if let Some(p) = &t.picture {
            println!("  picture {p:?}\n  coding {}", hex(&p.coding));
        }
        if let Some(s) = &t.sound {
            println!("  sound {s:?}");
        }
        println!("  {}x{} par {:?} ts {} ch {} rate {} samples {}", tr.width, tr.height, tr.par, tr.timescale, tr.channels, tr.sample_rate, tr.samples.len());
        for (i, s) in tr.samples.iter().take(n).enumerate() {
            println!("    {i}: off {} size {} dts {} pts {} key {}", s.offset, s.size, s.dts, s.pts, s.key);
        }
    }
}
