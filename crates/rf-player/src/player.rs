//! Oynatıcı: saat iş parçacığı (ses besleme + konum) ve çözme iş parçacığı (önde 8 karelik kuyruk).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio_out::AudioSink;
use crate::clock::Clock;
use crate::source::{Frame, Media};

/// Çözücünün önde tuttuğu kare sayısı.
const AHEAD: usize = 8;
/// Bir seferde çözülen ses karesi.
const AUDIO_CHUNK: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerState {
    pub playing: bool,
    /// Saniye.
    pub position: f64,
    pub duration: f64,
    pub ended: bool,
}

#[derive(Default)]
struct Ctl {
    playing: bool,
    looping: bool,
    volume: f32,
    muted: bool,
    position: f64,
    ended: bool,
    /// Saat iş parçacığının uygulayacağı atlama.
    seek: Option<f64>,
    /// Her atlamada artar: çözücü kuyruğunu atıp yeni konumdan başlar.
    generation: u64,
}

struct Shared {
    ctl: Mutex<Ctl>,
    queue: Mutex<VecDeque<Arc<Frame>>>,
    current: Mutex<Option<Arc<Frame>>>,
    stop: AtomicBool,
    duration: f64,
    frame_dur: f64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Player {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Player {
    pub fn new(media: Media, sink: Box<dyn AudioSink>) -> Player {
        let info = media.info();
        let fps = info.video.as_ref().map(|v| v.frame_rate.as_f64()).filter(|f| *f > 0.0).unwrap_or(25.0);
        let shared = Arc::new(Shared {
            ctl: Mutex::new(Ctl { volume: 1.0, ..Default::default() }),
            queue: Mutex::new(VecDeque::new()),
            current: Mutex::new(None),
            stop: AtomicBool::new(false),
            duration: media.duration(),
            frame_dur: 1.0 / fps,
        });
        let mut threads = Vec::new();
        if info.has_video() {
            let (s, m) = (shared.clone(), media.clone());
            threads.push(std::thread::Builder::new().name("player-video".into()).spawn(move || video_loop(&s, &m)).expect("player video thread"));
        }
        let s = shared.clone();
        threads.push(std::thread::Builder::new().name("player-clock".into()).spawn(move || clock_loop(&s, &media, sink)).expect("player clock thread"));
        Player { shared, threads }
    }

    pub fn play(&mut self) {
        let mut c = lock(&self.shared.ctl);
        if c.ended {
            c.seek = Some(0.0);
            c.position = 0.0;
            c.generation += 1;
            c.ended = false;
        }
        c.playing = true;
    }

    pub fn pause(&mut self) {
        lock(&self.shared.ctl).playing = false;
    }

    pub fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.shared.duration.max(0.0));
        let mut c = lock(&self.shared.ctl);
        c.seek = Some(t);
        c.position = t;
        c.generation += 1;
        c.ended = false;
    }

    pub fn set_loop(&mut self, on: bool) {
        lock(&self.shared.ctl).looping = on;
    }

    pub fn set_volume(&mut self, v: f32) {
        lock(&self.shared.ctl).volume = v.clamp(0.0, 1.0);
    }

    pub fn set_mute(&mut self, on: bool) {
        lock(&self.shared.ctl).muted = on;
    }

    pub fn state(&self) -> PlayerState {
        let c = lock(&self.shared.ctl);
        PlayerState { playing: c.playing, position: c.position, duration: self.shared.duration, ended: c.ended }
    }

    /// Saat konumunda gösterilmesi gereken kare (yoksa en son gösterilen).
    pub fn current_frame(&self) -> Option<Arc<Frame>> {
        let pos = lock(&self.shared.ctl).position;
        let eps = self.shared.frame_dur * 0.5;
        let mut q = lock(&self.shared.queue);
        while q.len() >= 2 && q[1].pts <= pos + eps {
            q.pop_front();
        }
        let mut cur = lock(&self.shared.current);
        if let Some(f) = q.front().filter(|f| f.pts <= pos + eps) {
            *cur = Some(f.clone());
        }
        cur.clone()
    }
}

impl Drop for Player {
    /// İş parçacıkları beklenmez (biri büyük bir dosyada uzun bir atlamanın içinde olabilir;
    /// pencere kapanırken arayüz donmasın): durma işareti verilir, kendi paylaşımlarıyla biterler.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.threads.clear();
    }
}

fn video_loop(s: &Shared, media: &Media) {
    let fd = s.frame_dur;
    let mut seen_gen = u64::MAX;
    let mut next = 0.0f64;
    while !s.stop.load(Ordering::Relaxed) {
        let (g, pos, playing) = {
            let c = lock(&s.ctl);
            (c.generation, c.position, c.playing)
        };
        if g != seen_gen {
            seen_gen = g;
            lock(&s.queue).clear();
            next = pos;
        }
        // Geride kaldıysak geç kareleri hiç çözmeden atla.
        if next + fd < pos {
            next = pos;
        }
        let full = lock(&s.queue).len() >= AHEAD;
        if full || next >= s.duration {
            // nothing to decode: while paused the queue only moves on a seek, look less often
            std::thread::sleep(Duration::from_millis(if playing { 3 } else { 15 }));
            continue;
        }
        let idx = (next / fd + 1e-6).floor();
        let Ok(mut f) = media.frame_at((idx + 0.5) * fd) else {
            next = (idx + 1.0) * fd;
            continue;
        };
        if !f.pts.is_finite() || f.pts < 0.0 {
            f.pts = idx * fd;
        }
        if lock(&s.ctl).generation == seen_gen {
            let mut q = lock(&s.queue);
            if q.back().is_none_or(|b| b.pts < f.pts) {
                q.push_back(Arc::new(f));
            }
        }
        next = (idx + 1.0) * fd;
    }
}

/// `src` içindeki `from` kanallı araya karışık sesi `to` kanala çevirir.
fn map_channels(src: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == to || from == 0 {
        return src.to_vec();
    }
    let frames = src.len() / from;
    let mut out = Vec::with_capacity(frames * to);
    for f in src.chunks_exact(from) {
        if from == 1 {
            out.extend(std::iter::repeat_n(f[0], to));
        } else if to == 2 && from >= 6 {
            // 5.1 → stereo (FL FR C LFE BL BR)
            let k = std::f32::consts::FRAC_1_SQRT_2;
            let norm = 1.0 / (1.0 + 2.0 * k);
            out.push((f[0] + k * f[2] + k * f[4]) * norm);
            out.push((f[1] + k * f[2] + k * f[5]) * norm);
        } else {
            out.extend((0..to).map(|c| if c < from { f[c] } else { 0.0 }));
        }
    }
    out
}

fn clock_loop(s: &Shared, media: &Media, mut sink: Box<dyn AudioSink>) {
    let audio_channels = media.info().audio().map(|a| a.channels as usize);
    let rate = sink.sample_rate();
    let out_ch = sink.channels() as usize;
    // Saat çapası: medya zamanı `anchor` iken ses sayacı `anchor_played` ya da duvar saati `anchor_at`.
    let mut anchor = 0.0f64;
    let mut anchor_played = sink.played_frames();
    let mut anchor_at: Option<Instant> = None;
    let mut next_sample: i64 = 0;
    let mut pending: Vec<f32> = Vec::new();
    let mut playing = false;
    let mut applied_volume = -1.0f32;
    sink.pause(true);
    while !s.stop.load(Ordering::Relaxed) {
        let (want_play, seek, looping, vol) = {
            let mut c = lock(&s.ctl);
            (c.playing, c.seek.take(), c.looping, if c.muted { 0.0 } else { c.volume })
        };
        if vol != applied_volume {
            sink.set_volume(vol);
            applied_volume = vol;
        }
        let pos_now = |sink: &dyn AudioSink, anchor_at: Option<Instant>, anchor: f64, anchor_played: u64| -> f64 {
            if audio_channels.is_some() {
                anchor + (Clock::audio(sink).now() - anchor_played as f64 / rate as f64)
            } else {
                anchor + anchor_at.map_or(0.0, |t| Clock::wall(t).now())
            }
        };
        if let Some(t) = seek {
            sink.flush();
            pending.clear();
            anchor = t;
            anchor_played = sink.played_frames();
            anchor_at = playing.then(Instant::now);
            next_sample = (t * rate as f64).round() as i64;
        }
        if want_play != playing {
            if want_play {
                anchor = pos_now(&*sink, anchor_at, anchor, anchor_played);
                anchor_played = sink.played_frames();
                anchor_at = Some(Instant::now());
            } else {
                anchor = pos_now(&*sink, anchor_at, anchor, anchor_played);
                anchor_played = sink.played_frames();
                anchor_at = None;
            }
            sink.pause(!want_play);
            playing = want_play;
        }
        if playing && let Some(mc) = audio_channels {
            loop {
                if pending.is_empty() {
                    if next_sample as f64 / rate as f64 >= s.duration {
                        break;
                    }
                    match media.audio(next_sample, AUDIO_CHUNK, rate) {
                        Ok(a) => {
                            next_sample += (a.len() / mc.max(1)) as i64;
                            pending = map_channels(&a, mc, out_ch);
                        }
                        Err(_) => break,
                    }
                    if pending.is_empty() {
                        break;
                    }
                }
                let w = sink.write(&pending);
                pending.drain(..w * out_ch);
                if !pending.is_empty() {
                    break;
                }
            }
        }
        let mut pos = pos_now(&*sink, anchor_at, anchor, anchor_played).max(0.0);
        let mut c = lock(&s.ctl);
        if c.seek.is_none() {
            if playing && pos >= s.duration {
                if looping {
                    c.seek = Some(0.0);
                    c.generation += 1;
                    pos = 0.0;
                } else {
                    pos = s.duration;
                    c.playing = false;
                    c.ended = true;
                }
            }
            if playing || seek.is_some() {
                c.position = pos;
            }
        }
        drop(c);
        // paused: the clock does not move, only a play or a seek wakes it (each read on the next
        // look); playing keeps the audio fed
        std::thread::sleep(Duration::from_millis(if playing || seek.is_some() { 4 } else { 20 }));
    }
}
