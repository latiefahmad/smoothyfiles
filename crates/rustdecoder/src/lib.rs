//! rustdecoder: smoothyfiles' own video engine. Containers and bitstreams are parsed here; the
//! compressed pictures go to the graphics card's video engine (D3D11 Video). See
//! `docs/design/rustdecoder.md`.

pub mod annexb;
pub mod audio;
pub mod avi;
pub mod av1;
pub mod dxva_av1;
pub mod bits;
pub mod flac;
pub mod dpb;
pub mod h264;
pub mod h264_dpb;
pub mod hevc;
pub mod container;
pub mod mkv;
pub mod mp4;
pub mod mpeg2;
pub mod ps;
pub mod ts;
pub mod mxf;
pub mod nal;
pub mod dnx;
pub mod dnxtables;
pub mod prores;
#[cfg(windows)]
pub mod dnxdec;
#[cfg(windows)]
pub mod proresdec;
pub mod resample;
pub mod slice;

#[cfg(windows)]
pub mod audio_out;
#[cfg(windows)]
pub mod d3d11;
#[cfg(windows)]
pub mod decoder;
pub mod dxva;
#[cfg(windows)]
pub mod player;
#[cfg(windows)]
pub mod playback;
#[cfg(windows)]
pub mod present;
pub mod dxva_h264;
pub mod dxva_vp9;
pub mod vp9;
#[cfg(windows)]
pub mod av1dec;
#[cfg(windows)]
pub mod vp9dec;
pub mod dxva_mpeg2;
#[cfg(windows)]
pub mod mpeg2dec;
#[cfg(windows)]
pub mod h264dec;
#[cfg(windows)]
pub mod video;
#[cfg(windows)]
pub mod window;

#[cfg(windows)]
pub use decoder::{Frame, HevcDecoder, StreamInfo};
#[cfg(windows)]
pub use h264dec::H264Decoder;
#[cfg(windows)]
pub use vp9dec::Vp9Decoder;
#[cfg(windows)]
pub use av1dec::Av1Decoder;
#[cfg(windows)]
pub use video::VideoDecoder;

/// Errors from reading a file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file breaks the format (a box runs past its parent, a table is cut short, …).
    #[error("invalid file: {0}")]
    Invalid(&'static str),
    /// Valid, but something this reader doesn't handle yet.
    #[error("not supported yet: {0}")]
    Unsupported(&'static str),
    /// The graphics driver refused something.
    #[error("GPU: {0}")]
    Gpu(String),
}

pub type Result<T> = std::result::Result<T, Error>;
