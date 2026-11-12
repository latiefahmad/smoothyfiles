//! `VideoDecoder`: open a clip and get its pictures whatever the codec (H.264 or HEVC).

use std::fs::File;
use std::path::Path;

use crate::d3d11::{Gpu, Planes};
use crate::decoder::{Frame, HevcDecoder, StreamInfo};
use crate::h264dec::H264Decoder;
use crate::dnxdec::DnxVideo;
use crate::proresdec::ProResVideo;
use crate::vp9dec::Vp9Decoder;
use crate::av1dec::Av1Decoder;
use crate::mp4::{Codec, Track};
use crate::{Error, Result};

pub enum VideoDecoder {
    H264(H264Decoder),
    Hevc(HevcDecoder),
    Vp9(Vp9Decoder),
    Av1(Av1Decoder),
    ProRes(ProResVideo),
    Dnx(DnxVideo),
    Mpeg2(crate::mpeg2dec::Mpeg2Decoder),
}

macro_rules! each {
    ($s:expr, $d:ident => $e:expr) => {
        match $s {
            VideoDecoder::H264($d) => $e,
            VideoDecoder::Hevc($d) => $e,
            VideoDecoder::Vp9($d) => $e,
            VideoDecoder::Av1($d) => $e,
            VideoDecoder::ProRes($d) => $e,
            VideoDecoder::Dnx($d) => $e,
            VideoDecoder::Mpeg2($d) => $e,
        }
    };
}

impl VideoDecoder {
    /// Opens the first video track of an MP4 / MOV / Matroska / WebM file with the decoder its codec needs.
    pub fn open(path: &Path) -> Result<Self> {
        let movie = crate::container::read(&mut File::open(path)?)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?;
        match track.codec {
            Codec::H264 { .. } => Ok(Self::H264(H264Decoder::open(path)?)),
            Codec::Hevc { .. } => Ok(Self::Hevc(HevcDecoder::open(path)?)),
            Codec::Vp9 { .. } => Ok(Self::Vp9(Vp9Decoder::open(path)?)),
            Codec::Av1 { .. } => Ok(Self::Av1(Av1Decoder::open(path)?)),
            _ if &track.fourcc == b"mpg2" || &track.fourcc == b"mpg1" => Ok(Self::Mpeg2(crate::mpeg2dec::Mpeg2Decoder::open(path)?)),
            _ if crate::prores::is_prores_fourcc(&track.fourcc) => Ok(Self::ProRes(ProResVideo::open(path)?)),
            _ if crate::dnx::is_dnx_fourcc(&track.fourcc) => Ok(Self::Dnx(DnxVideo::open(path)?)),
            _ => Err(Error::Unsupported("video codec other than H.264, HEVC, VP9, AV1, ProRes and DNxHD / DNxHR")),
        }
    }

    pub fn codec_name(&self) -> &'static str {
        match self {
            Self::H264(_) => "H.264",
            Self::Hevc(_) => "HEVC",
            Self::Vp9(_) => "VP9",
            Self::Av1(_) => "AV1",
            Self::ProRes(_) => "ProRes",
            Self::Dnx(_) => "DNxHR",
            Self::Mpeg2(_) => "MPEG-2",
        }
    }

    pub fn info(&self) -> &StreamInfo {
        each!(self, d => d.info())
    }

    /// The device the pictures live on.
    pub fn gpu(&self) -> &Gpu {
        each!(self, d => d.gpu())
    }

    /// The video track with its sample table.
    pub fn track(&self) -> &Track {
        each!(self, d => d.track())
    }

    /// Continues decoding at sync sample `i` (see the codec's `seek_to_sample`).
    pub fn seek_to_sample(&mut self, i: usize) {
        each!(self, d => d.seek_to_sample(i))
    }

    pub fn adapter(&self) -> &str {
        each!(self, d => d.adapter())
    }

    pub fn config_bitstream_raw(&self) -> Option<u32> {
        each!(self, d => d.config_bitstream_raw())
    }

    pub fn wait_idle(&self) -> Result<()> {
        each!(self, d => d.wait_idle())
    }

    /// The next picture in display order; `None` at the end of the stream. A frame stays valid
    /// until the next call.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        each!(self, d => d.next_frame())
    }

    pub fn read_back(&mut self, f: &Frame) -> Result<Planes> {
        each!(self, d => d.read_back(f))
    }
}
