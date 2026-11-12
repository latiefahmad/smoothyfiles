//! Decodes the first audio track of a file and prints what came out: `audio_dump <file>`.
#[cfg(windows)]
fn main() {
    let path = std::env::args().nth(1).expect("usage: audio_dump <file>");
    eprintln!("opening");
    let mut d = match rustdecoder::audio::AudioDecoder::open(std::path::Path::new(&path)) {
        Ok(Some(d)) => d,
        Ok(None) => return println!("no audio track"),
        Err(e) => return println!("open failed: {e}"),
    };
    println!("{:?}", d.info());
    let (mut blocks, mut frames, mut first) = (0, 0usize, None);
    let ch = d.info().channels as usize;
    loop {
        match d.next_block() {
            Ok(Some(b)) => {
                first.get_or_insert(b.pts);
                blocks += 1;
                frames += b.samples.len() / ch;
                if blocks <= 3 {
                    eprintln!("block {blocks}: pts {:.6}, {} frames", b.pts, b.samples.len() / ch);
                }
            }
            Ok(None) => break,
            Err(e) => return println!("decode failed: {e}"),
        }
    }
    println!("{blocks} blocks, {frames} frames, first pts {:?}", first);
}
#[cfg(not(windows))]
fn main() {}
