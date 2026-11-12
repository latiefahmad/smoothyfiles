//! Lists the audio decoders (Media Foundation transforms) this machine has for each codec.
#[cfg(windows)]
fn main() {
    for (name, found) in rustdecoder::audio::installed_decoders() {
        println!("{name:8} {}", if found.is_empty() { "none".to_string() } else { found.join(", ") });
    }
}
#[cfg(not(windows))]
fn main() {}
