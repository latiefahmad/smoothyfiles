//! Dosyayı FilmCraft ile açma, kare ve ses okuma.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use filmcraft_media::reader::{FileReader, open_reader_within};
use filmcraft_media::{FrameRequest, MediaInfo, SharedSource};
use filmcraft_time::Tick;

/// Kare pikselleri (FilmCraft'ın biçimi: 8/16 bit YUV düzlemleri ya da RGBA).
pub type Planes = filmcraft_frame::PixelData;

/// Akışla okunmayan biçimler en çok bu kadar bayta kadar belleğe alınır.
const WHOLE_FILE_MAX: u64 = 64 << 20;

#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub planes: Planes,
    pub color: filmcraft_color::ColorInfo,
    /// Saniye.
    pub pts: f64,
}

impl Frame {
    /// Tek renkli 8 bit 4:2:0 kare (BT.709, sınırlı aralık); testler ve yer tutucular için.
    pub fn yuv420_solid(width: u32, height: u32, y: u8, u: u8, v: u8) -> Frame {
        let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
        let planes = [Arc::new(vec![y; (width * height) as usize]), Arc::new(vec![u; cw * ch]), Arc::new(vec![v; cw * ch])];
        Frame {
            width,
            height,
            planes: Planes::Yuv8 { planes, chroma: filmcraft_frame::Chroma::C420, alpha: None },
            color: filmcraft_color::ColorInfo::REC709,
            pts: 0.0,
        }
    }
}

impl Frame {
    /// Tek renkli 16 bit kaplı (`bits` anlamlı) 4:2:0 kare (BT.709, sınırlı aralık); testler için.
    pub fn yuv420_solid_16(width: u32, height: u32, bits: u32, y: u16, u: u16, v: u16) -> Frame {
        let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
        let planes = [Arc::new(vec![y; (width * height) as usize]), Arc::new(vec![u; cw * ch]), Arc::new(vec![v; cw * ch])];
        Frame {
            width,
            height,
            planes: Planes::Yuv16 { planes, chroma: filmcraft_frame::Chroma::C420, bits, alpha: None },
            color: filmcraft_color::ColorInfo::REC709,
            pts: 0.0,
        }
    }
}

/// Açılmış bir video ya da ses dosyası.
#[derive(Clone)]
pub struct Media {
    src: SharedSource,
}

/// Dosyayı açar: yalnız dizini okunur, kareler gerektikçe gelir.
pub fn open(path: &Path) -> anyhow::Result<Media> {
    let reader = FileReader::open(path).with_context(|| path.display().to_string())?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let src = open_reader_within(&name, Arc::new(reader), &filmcraft_codecs::reader_openers(), &filmcraft_codecs::openers(), WHOLE_FILE_MAX)
        .map_err(|e| anyhow!("{name}: {e}"))?;
    Ok(Media { src })
}

/// Uzantısına göre bu oynatıcının deneyeceği bir dosya mı.
pub fn can_open(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    filmcraft_media::VIDEO_EXTENSIONS.contains(&ext.as_str()) || filmcraft_media::AUDIO_EXTENSIONS.contains(&ext.as_str())
}

impl Media {
    pub fn info(&self) -> &MediaInfo {
        self.src.info()
    }

    /// Süre (saniye).
    pub fn duration(&self) -> f64 {
        self.info().duration.seconds()
    }

    /// `t` saniyesinde gösterilen kare.
    pub fn frame_at(&self, t: f64) -> anyhow::Result<Frame> {
        let f = self.src.video_frame(FrameRequest::full(Tick::from_seconds_f64(t))).map_err(|e| anyhow!("{e}"))?;
        // FilmCraft'ın kare zamanı yalnız bilgi amaçlı (çoğu kaynakta 0): `t`'yi kapsayan karenin başı hesaplanır.
        let pts = match self.info().video.as_ref().map(|v| v.frame_rate.as_f64()).filter(|r| *r > 0.0) {
            Some(fps) => (t * fps + 1e-6).floor() / fps,
            None => f.pts.seconds(),
        };
        Ok(Frame { width: f.width, height: f.height, planes: f.data.clone(), color: f.color, pts })
    }

    /// `start_sample`'dan başlayan `frames` ses karesi, `rate` örnekleme hızında, kanallar araya karışık.
    pub fn audio(&self, start_sample: i64, frames: usize, rate: u32) -> anyhow::Result<Vec<f32>> {
        if !self.info().has_audio() {
            return Err(anyhow!("no audio stream"));
        }
        let buf = self.src.audio(start_sample, frames, rate).map_err(|e| anyhow!("{e}"))?;
        let ch = buf.channel_count();
        let n = buf.frames();
        let mut out = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in &buf.channels {
                out.push(c[i]);
            }
        }
        Ok(out)
    }
}
