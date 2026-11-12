//! Ses çıkışı: WASAPI (paylaşımlı mod) ve testler için sessiz, gerçek zamanlı ilerleyen `NullSink`.

use std::time::Instant;

/// Oynatıcının sesi yazdığı yer. `write` alabildiği kadarını alır (kare sayısı döner); `played_frames`
/// hoparlörden çıkmış kare sayısıdır ve oynatma saati bundan hesaplanır.
pub trait AudioSink: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn write(&mut self, interleaved: &[f32]) -> usize;
    fn played_frames(&self) -> u64;
    fn pause(&mut self, on: bool);
    fn set_volume(&mut self, v: f32);
    /// Kuyruktaki çalınmamış sesi atar (atlamada); `played_frames` sayacı korunur.
    fn flush(&mut self) {}
}

/// Sessiz çıkış: yazılanı gerçek zamanda "çalar" (yazılandan ileri gitmez). Arabellek 200 ms.
pub struct NullSink {
    rate: u32,
    channels: u16,
    written: u64,
    /// Duraklatıldığında ya da kuyruk bittiğinde donmuş çalınan sayı.
    base: u64,
    since: Option<Instant>,
    manual: u64,
}

impl NullSink {
    pub fn new(rate: u32, channels: u16) -> NullSink {
        NullSink { rate, channels, written: 0, base: 0, since: None, manual: 0 }
    }

    /// Testler için: saati elle ilerletir.
    pub fn advance_frames(&mut self, n: u64) {
        self.manual += n;
    }

    fn realtime(&self) -> u64 {
        let run = self.since.map_or(0, |t| (t.elapsed().as_secs_f64() * self.rate as f64) as u64);
        (self.base + run).min(self.written)
    }
}

impl AudioSink for NullSink {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn write(&mut self, interleaved: &[f32]) -> usize {
        let frames = (interleaved.len() / self.channels.max(1) as usize) as u64;
        let queued = self.written - self.realtime();
        let room = (self.rate as u64 / 5).saturating_sub(queued);
        let n = frames.min(room);
        if self.since.is_some() && self.realtime() == self.written {
            // Kuyruk boşalmıştı: saat yeni veriden başlar.
            self.base = self.written;
            self.since = Some(Instant::now());
        }
        self.written += n;
        n as usize
    }
    fn played_frames(&self) -> u64 {
        self.manual + self.realtime()
    }
    fn pause(&mut self, on: bool) {
        if on {
            self.base = self.realtime();
            self.since = None;
        } else if self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }
    fn set_volume(&mut self, _v: f32) {}
    fn flush(&mut self) {
        self.base = self.realtime();
        self.written = self.base;
        if self.since.is_some() {
            self.since = Some(Instant::now());
        }
    }
}

pub use wasapi::wasapi_sink;

mod wasapi {
    use super::AudioSink;
    use anyhow::Context;
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
        IAudioClock, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};

    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    /// Arabellek süresi (100 ns birimi): 100 ms.
    const BUFFER_HNS: i64 = 1_000_000;

    struct Wasapi {
        client: IAudioClient,
        render: IAudioRenderClient,
        clock: IAudioClock,
        clock_freq: u64,
        buffer_frames: u32,
        rate: u32,
        channels: u16,
        volume: f32,
        started: bool,
        paused: bool,
        /// Atlamada temizlenen kuyruk saat konumunu geri götürmesin diye eklenen düzeltme.
        played_offset: u64,
    }

    // WASAPI nesneleri serbest iş parçacıklıdır (MTA); çıkış tek bir iş parçacığından sürülür.
    unsafe impl Send for Wasapi {}

    /// Varsayılan hoparlöre paylaşımlı modda açılan çıkış (aygıtın örnekleme hızı ve kanal sayısıyla, 32 bit kayan nokta).
    pub fn wasapi_sink() -> anyhow::Result<Box<dyn AudioSink>> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("MMDeviceEnumerator")?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).context("default audio endpoint")?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).context("IAudioClient")?;
            let mix = client.GetMixFormat()?;
            let (rate, channels) = ((*mix).nSamplesPerSec, (*mix).nChannels);
            CoTaskMemFree(Some(mix as *const _));
            let block = channels * 4;
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * block as u32,
                nBlockAlign: block,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_HNS,
                0,
                &fmt,
                None,
            )?;
            let buffer_frames = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            let clock: IAudioClock = client.GetService()?;
            let clock_freq = clock.GetFrequency()?;
            Ok(Box::new(Wasapi {
                client,
                render,
                clock,
                clock_freq,
                buffer_frames,
                rate,
                channels,
                volume: 1.0,
                started: false,
                paused: true,
                played_offset: 0,
            }))
        }
    }

    impl Wasapi {
        fn device_played(&self) -> u64 {
            let mut pos = 0u64;
            if unsafe { self.clock.GetPosition(&mut pos, None) }.is_err() || self.clock_freq == 0 {
                return 0;
            }
            (pos as u128 * self.rate as u128 / self.clock_freq as u128) as u64
        }
    }

    impl AudioSink for Wasapi {
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
        fn write(&mut self, interleaved: &[f32]) -> usize {
            let ch = self.channels as usize;
            let padding = unsafe { self.client.GetCurrentPadding() }.unwrap_or(self.buffer_frames);
            let room = self.buffer_frames.saturating_sub(padding) as usize;
            let n = (interleaved.len() / ch).min(room);
            if n == 0 {
                return 0;
            }
            unsafe {
                let Ok(ptr) = self.render.GetBuffer(n as u32) else { return 0 };
                let out = std::slice::from_raw_parts_mut(ptr as *mut f32, n * ch);
                for (o, i) in out.iter_mut().zip(interleaved) {
                    *o = i * self.volume;
                }
                let _ = self.render.ReleaseBuffer(n as u32, 0);
                if !self.started && !self.paused {
                    let _ = self.client.Start();
                    self.started = true;
                }
            }
            n
        }
        fn played_frames(&self) -> u64 {
            self.device_played() + self.played_offset
        }
        fn pause(&mut self, on: bool) {
            self.paused = on;
            unsafe {
                if on {
                    let _ = self.client.Stop();
                    self.started = false;
                } else if !self.started {
                    let _ = self.client.Start();
                    self.started = true;
                }
            }
        }
        fn set_volume(&mut self, v: f32) {
            self.volume = v.clamp(0.0, 1.0);
        }
        fn flush(&mut self) {
            let before = self.played_frames();
            unsafe {
                let _ = self.client.Stop();
                let _ = self.client.Reset();
            }
            // Reset aygıt saatini sıfırlar: sayaç geri gitmesin.
            self.played_offset = before.saturating_sub(self.device_played());
            self.started = false;
            if !self.paused {
                unsafe {
                    let _ = self.client.Start();
                }
                self.started = true;
            }
        }
    }

    impl Drop for Wasapi {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}
