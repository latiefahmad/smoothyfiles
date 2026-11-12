//! Lists the decoder profiles the video engine offers, with the MPEG ones named.

#[cfg(windows)]
fn main() {
    use windows::Win32::Graphics::Direct3D11::*;
    let g = rustdecoder::d3d11::Gpu::open_any().expect("gpu");
    println!("adapter: {}", g.adapter_name);
    unsafe {
        let n = g.video_device.GetVideoDecoderProfileCount();
        for i in 0..n {
            let p = g.video_device.GetVideoDecoderProfile(i).unwrap();
            let name = if p == D3D11_DECODER_PROFILE_MPEG2_VLD {
                "MPEG2_VLD"
            } else if p == D3D11_DECODER_PROFILE_MPEG2and1_VLD {
                "MPEG2and1_VLD"
            } else if p == D3D11_DECODER_PROFILE_MPEG2_IDCT {
                "MPEG2_IDCT"
            } else if p == D3D11_DECODER_PROFILE_MPEG2_MOCOMP {
                "MPEG2_MOCOMP"
            } else {
                ""
            };
            println!("{p:?} {name}");
        }
    }
}

#[cfg(not(windows))]
fn main() {}
