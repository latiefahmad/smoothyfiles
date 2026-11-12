//! Playback core: a worker thread decodes ahead into a small ring of surfaces, a clock says
//! what time it is in the movie, and [`Player::frame_for`] picks the picture to show.
//!
//! ```text
//!   worker thread                                  caller's thread
//!   VideoDecoder::next_frame ─► copy into ring ─► channel ─► Player::frame_for(now) ─► Shown
//!        ▲                         slot                           │
//!        └───────── free slots ◄──────────────────────────────────┘
//! ```
//!
//! The ring has a fixed number of slots (one texture array). The worker takes a free slot for
//! every picture, so it never runs more than `ring` pictures ahead (counting the one on show);
//! the caller gives a slot back when the picture is replaced. Frames come out of the decoder in
//! presentation order, so the queue is sorted by time.
//!
//! Seeking jumps to a sync sample before the target (one more sync sample back, so the RASL
//! pictures of a CRA picture decode), decodes forward without copying and drops every picture
//! before the one the target falls on.
//!
//! Immediate context calls of the decoder thread (decode, copy) and of the caller (shader
//! convert) are serialised by D3D11's multithread protection, which the player switches on.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::{Interface};

use crate::decoder::StreamInfo;
use crate::video::VideoDecoder;
use crate::present::Source;
use crate::{Error, Result};

// ---------------------------------------------------------------------------------------------
// clock

/// Where the clock reads wall time from: seconds on a monotonic scale. Tests use [`FakeTime`].
pub trait TimeSource: Send + Sync {
    fn now(&self) -> f64;
}

/// The system's monotonic clock.
pub struct SystemTime(Instant);

impl SystemTime {
    pub fn new() -> Self {
        SystemTime(Instant::now())
    }
}

impl Default for SystemTime {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for SystemTime {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

/// A time that only moves when told to.
#[derive(Default)]
pub struct FakeTime(AtomicU64);

impl FakeTime {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, t: f64) {
        self.0.store(t.to_bits(), Ordering::SeqCst);
    }
    pub fn advance(&self, dt: f64) {
        self.set(self.now() + dt);
    }
}

impl TimeSource for FakeTime {
    fn now(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::SeqCst))
    }
}

/// The playback clock: movie time in seconds, started, paused, resumed and moved by the caller.
pub struct Clock {
    source: Arc<dyn TimeSource>,
    media: f64,
    wall: f64,
    running: bool,
}

impl Clock {
    /// A paused clock at movie time 0.
    pub fn new(source: Arc<dyn TimeSource>) -> Self {
        let wall = source.now();
        Clock { source, media: 0.0, wall, running: false }
    }

    /// Movie time now.
    pub fn time(&self) -> f64 {
        if self.running { self.media + (self.source.now() - self.wall) } else { self.media }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Runs from movie time `media`.
    pub fn start(&mut self, media: f64) {
        self.media = media;
        self.wall = self.source.now();
        self.running = true;
    }

    pub fn pause(&mut self) {
        self.media = self.time();
        self.running = false;
    }

    pub fn resume(&mut self) {
        if !self.running {
            self.wall = self.source.now();
            self.running = true;
        }
    }

    /// Moves to movie time `media`, keeping the running state.
    pub fn seek(&mut self, media: f64) {
        self.media = media;
        self.wall = self.source.now();
    }
}

// ---------------------------------------------------------------------------------------------
// seek planning

/// Where to start decoding and which picture to land on for a seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekPlan {
    /// Sample (in decode order) to restart decoding at: a sync sample.
    pub start_sample: usize,
    /// Presentation time (track units) of the picture shown: the largest not above the target,
    /// or the first picture when the target is before it.
    pub landing_pts: i64,
}

/// `pts` are the presentation times of all samples in decode order, `keys` the sync samples.
pub fn plan_seek(pts: &[i64], keys: &[usize], target: i64) -> SeekPlan {
    let mut landing: Option<usize> = None;
    let mut first: Option<usize> = None;
    for (i, &p) in pts.iter().enumerate() {
        if first.is_none_or(|f| p < pts[f]) {
            first = Some(i);
        }
        if p <= target && landing.is_none_or(|l| p > pts[l]) {
            landing = Some(i);
        }
    }
    let Some(l) = landing.or(first) else { return SeekPlan { start_sample: 0, landing_pts: 0 } };
    // last sync sample at or before the landing sample in decode order, then one more back:
    // the RASL pictures of a CRA picture need the pictures before it
    let at = keys.partition_point(|&k| k <= l);
    let start_sample = match at {
        0 => keys.first().copied().unwrap_or(0),
        1 => keys[0],
        n => keys[n - 2],
    };
    SeekPlan { start_sample, landing_pts: pts[l] }
}

// ---------------------------------------------------------------------------------------------
// the worker

struct Ready {
    epoch: u64,
    slot: usize,
    pts: f64,
    texture: ID3D11Texture2D,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    bit_depth: u8,
}

enum Msg {
    Frame(Ready),
    Ended(u64),
    Failed(u64, String),
}

enum Cmd {
    Seek { epoch: u64, start: usize, skip_until: i64 },
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    decoded: AtomicU64,
    skipped: AtomicU64,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

struct SendBox<T>(T);
// SAFETY: the decoder is used from one thread at a time (it moves into the worker); D3D11
// devices are free threaded and the context is multithread protected (see the module comment).
unsafe impl<T> Send for SendBox<T> {}

fn make_ring(dec: &VideoDecoder, like: &ID3D11Texture2D, slots: usize) -> Result<ID3D11Texture2D> {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { like.GetDesc(&mut d) };
    // NV12 / P010 arrays: some drivers only accept the combinations they can decode into
    let mut last = String::new();
    for binds in [D3D11_BIND_SHADER_RESOURCE.0, D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_DECODER.0, D3D11_BIND_DECODER.0] {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: d.Width,
            Height: d.Height,
            MipLevels: 1,
            ArraySize: slots as u32,
            Format: d.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: binds as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t = None;
        match unsafe { dec.gpu().device.CreateTexture2D(&desc, None, Some(&mut t)) } {
            Ok(()) if t.is_some() => return Ok(t.unwrap()),
            Ok(()) => {}
            Err(e) => last = format!("{e} (binds {binds:#x}, format {}, {}x{}, {slots} slices)", d.Format.0, d.Width, d.Height),
        }
    }
    Err(Error::Gpu(format!("ring texture: {last}")))
}

struct Worker {
    dec: VideoDecoder,
    slots: usize,
    shared: Arc<Shared>,
    msg_tx: Sender<Msg>,
    cmd_rx: Receiver<Cmd>,
    free_rx: Receiver<usize>,
    free_tx: Sender<usize>,
    ring: Option<(ID3D11Texture2D, (u32, u32, i32))>,
}

impl Worker {
    fn run(mut self) {
        let mut epoch = 0u64;
        let mut skip_until = i64::MIN;
        let mut idle = false;
        loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                return;
            }
            // commands: the latest seek wins
            let mut seek = None;
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(c) => seek = Some(c),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => return,
                }
            }
            if let Some(Cmd::Seek { epoch: g, start, skip_until: s }) = seek {
                epoch = g;
                skip_until = s;
                self.dec.seek_to_sample(start);
                idle = false;
            }
            if idle {
                match self.cmd_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(Cmd::Seek { epoch: g, start, skip_until: s }) => {
                        epoch = g;
                        skip_until = s;
                        self.dec.seek_to_sample(start);
                        idle = false;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                continue;
            }
            let slot = match self.free_rx.recv_timeout(Duration::from_millis(5)) {
                Ok(s) => s,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let n = self.shared.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.shared.max_in_flight.fetch_max(n, Ordering::SeqCst);
            // the slot is ours: fill it, or give it back
            match self.fill(slot, epoch, skip_until) {
                Ok(true) => {}
                Ok(false) => {
                    self.give_back(slot);
                    let _ = self.msg_tx.send(Msg::Ended(epoch));
                    idle = true;
                }
                Err(e) => {
                    self.give_back(slot);
                    let _ = self.msg_tx.send(Msg::Failed(epoch, e.to_string()));
                    idle = true;
                }
            }
        }
    }

    /// The slot was never handed out: back to the free list.
    fn give_back(&self, slot: usize) {
        self.shared.in_flight.fetch_sub(1, Ordering::SeqCst);
        let _ = self.free_tx.send(slot);
    }

    /// Decodes until a picture at or after `skip_until` is out, copies it into `slot`.
    /// `false` at the end of the stream.
    fn fill(&mut self, slot: usize, epoch: u64, skip_until: i64) -> Result<bool> {
        loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let Some(f) = self.dec.next_frame()? else { return Ok(false) };
            self.shared.decoded.fetch_add(1, Ordering::Relaxed);
            if f.pts < skip_until {
                self.shared.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { f.texture.GetDesc(&mut d) };
            let key = (d.Width, d.Height, d.Format.0);
            if self.ring.is_none() {
                self.ring = Some((make_ring(&self.dec, &f.texture, self.slots)?, key));
            }
            let (ring, ring_key) = self.ring.as_ref().unwrap();
            if *ring_key != key {
                return Err(Error::Unsupported("picture size change inside a stream"));
            }
            unsafe { self.dec.gpu().context.CopySubresourceRegion(ring, slot as u32, 0, 0, 0, &f.texture, f.index, None) };
            let ts = self.dec.info().timescale.max(1) as f64;
            let ready = Ready { epoch, slot, pts: f.pts as f64 / ts, texture: ring.clone(), left: f.crop.0, top: f.crop.2, width: f.width, height: f.height, bit_depth: f.bit_depth };
            if self.msg_tx.send(Msg::Frame(ready)).is_err() {
                return Ok(false);
            }
            return Ok(true);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// the player

/// A picture on show: a slice of the ring texture. Valid until the next call of
/// [`Player::frame_for`] that returns a different `serial`, or [`Player::seek`].
#[derive(Debug, Clone)]
pub struct Shown {
    /// Increases with every distinct picture; unchanged while the same one stays on show.
    pub serial: u64,
    pub texture: ID3D11Texture2D,
    pub slice: u32,
    /// Presentation time in seconds.
    pub pts: f64,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
}

impl Shown {
    pub fn source(&self) -> Source {
        Source { texture: self.texture.clone(), slice: self.slice, left: self.left, top: self.top, width: self.width, height: self.height, bit_depth: self.bit_depth }
    }
}

/// Counters since the player was opened.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// Pictures the decoder produced (including those a seek dropped).
    pub decoded: u64,
    /// Pictures dropped by a seek, before the one it landed on (never copied).
    pub skipped_by_seek: u64,
    /// Pictures that were due but replaced by a later due one before they were shown.
    pub dropped: u64,
    /// Pictures shown more than one frame duration after their time.
    pub late: u64,
    /// Distinct pictures handed out by `frame_for`.
    pub shown: u64,
    /// Most ring slots in use at once (decoded ahead, queued, or on show).
    pub max_in_flight: usize,
    pub ring: usize,
}

struct Current {
    ready: Ready,
    returned: bool,
}

pub struct Player {
    info: StreamInfo,
    codec: &'static str,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub clock: Clock,
    timescale: f64,
    pts: Vec<i64>,
    keys: Vec<usize>,
    start_time: f64,
    end_time: f64,
    frame_duration: f64,
    ring: usize,
    epoch: u64,
    shared: Arc<Shared>,
    msg_rx: Receiver<Msg>,
    cmd_tx: Sender<Cmd>,
    free_tx: Sender<usize>,
    pending: VecDeque<Ready>,
    current: Option<Current>,
    ended: bool,
    failed: Option<String>,
    serial: u64,
    dropped: u64,
    late: u64,
    shown: u64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Player {
    /// Opens the file and starts decoding ahead into `ring` slots (at least 2). The clock starts
    /// paused at the time of the first picture.
    pub fn open(path: &Path, ring: usize) -> Result<Player> {
        Self::open_with(path, ring, Arc::new(SystemTime::new()))
    }

    /// Like [`Player::open`], with a given time source for the clock.
    pub fn open_with(path: &Path, ring: usize, time: Arc<dyn TimeSource>) -> Result<Player> {
        let ring = ring.max(2);
        let dec = VideoDecoder::open(path)?;
        let device = dec.gpu().device.clone();
        let context = dec.gpu().context.clone();
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let info = dec.info().clone();
        let codec = dec.codec_name();
        let track = dec.track();
        let timescale = track.timescale.max(1) as f64;
        let pts: Vec<i64> = track.samples.iter().map(|s| s.pts).collect();
        let keys: Vec<usize> = track.samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i).collect();
        let first = pts.iter().copied().min().unwrap_or(0);
        let last = pts.iter().copied().max().unwrap_or(0);
        let frame_duration = if info.frame_rate > 0.0 { 1.0 / info.frame_rate } else { 1.0 / 30.0 };
        let shared = Arc::new(Shared::default());
        let (msg_tx, msg_rx) = channel();
        let (cmd_tx, cmd_rx) = channel();
        let (free_tx, free_rx) = channel();
        for s in 0..ring {
            let _ = free_tx.send(s);
        }
        let worker = Worker { dec, slots: ring, shared: shared.clone(), msg_tx, cmd_rx, free_rx, free_tx: free_tx.clone(), ring: None };
        let boxed = SendBox(worker);
        let thread = std::thread::Builder::new()
            .name("rustdecoder-decode".into())
            .spawn(move || {
                let b = boxed;
                b.0.run();
            })
            .map_err(Error::Io)?;
        let mut clock = Clock::new(time);
        clock.seek(first as f64 / timescale);
        Ok(Player {
            info,
            codec,
            device,
            context,
            clock,
            timescale,
            pts,
            keys,
            start_time: first as f64 / timescale,
            end_time: last as f64 / timescale + frame_duration,
            frame_duration,
            ring,
            epoch: 0,
            shared,
            msg_rx,
            cmd_tx,
            free_tx,
            pending: VecDeque::new(),
            current: None,
            ended: false,
            failed: None,
            serial: 0,
            dropped: 0,
            late: 0,
            shown: 0,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// "H.264" or "HEVC".
    pub fn codec_name(&self) -> &'static str {
        self.codec
    }

    /// The device pictures live on, for a [`crate::present::Converter`].
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Lets the movie start at `t` when that is before the first picture (a sound track that
    /// starts earlier: B-frames without an edit list put the first picture a frame or two after
    /// the first sound). Until the first picture's time nothing is shown.
    pub fn start_earlier(&mut self, t: f64) {
        if t < self.start_time {
            self.start_time = t;
            self.clock.seek(t);
        }
    }

    /// Movie time of the first picture, and the end (last picture plus one frame), in seconds.
    pub fn range(&self) -> (f64, f64) {
        (self.start_time, self.end_time)
    }

    /// The error that stopped decoding, if any.
    pub fn error(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Every picture up to the end has been taken from the queue.
    pub fn finished(&self) -> bool {
        self.ended && self.pending.is_empty()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            decoded: self.shared.decoded.load(Ordering::Relaxed),
            skipped_by_seek: self.shared.skipped.load(Ordering::Relaxed),
            dropped: self.dropped,
            late: self.late,
            shown: self.shown,
            max_in_flight: self.shared.max_in_flight.load(Ordering::Relaxed),
            ring: self.ring,
        }
    }

    /// Slots in use right now.
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    fn release(&mut self, slot: usize) {
        self.shared.in_flight.fetch_sub(1, Ordering::SeqCst);
        let _ = self.free_tx.send(slot);
    }

    /// Takes in what the worker has produced.
    fn pump(&mut self) {
        while let Ok(m) = self.msg_rx.try_recv() {
            match m {
                Msg::Frame(r) if r.epoch == self.epoch => self.pending.push_back(r),
                Msg::Frame(r) => self.release(r.slot),
                Msg::Ended(g) if g == self.epoch => self.ended = true,
                Msg::Failed(g, e) if g == self.epoch => {
                    self.failed = Some(e);
                    self.ended = true;
                }
                _ => {}
            }
        }
    }

    /// Blocks (up to `timeout`) until the queue holds a picture after `now`, so that the one to
    /// show at `now` is known; or the stream ended, or the ring is full of pictures at or before
    /// `now`. For tests and headless runs; a window loop just calls [`Player::frame_for`].
    pub fn wait_ahead(&mut self, now: f64, timeout: Duration) -> bool {
        let t0 = Instant::now();
        loop {
            self.pump();
            let full = self.pending.len() + self.current.is_some() as usize >= self.ring;
            if self.ended || full || self.pending.iter().any(|r| r.pts > now + 1e-9) {
                return true;
            }
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// The picture to show at movie time `now`: the latest one whose time has come, never one
    /// from the future. Pictures that were due and are passed over count as dropped. `None`
    /// before the first picture is due (or still decoding).
    pub fn frame_for(&mut self, now: f64) -> Option<Shown> {
        self.pump();
        while self.pending.front().is_some_and(|r| r.pts <= now + 1e-9) {
            let next = self.pending.pop_front().unwrap();
            if let Some(old) = self.current.take() {
                if !old.returned {
                    self.dropped += 1;
                }
                self.release(old.ready.slot);
            }
            self.current = Some(Current { ready: next, returned: false });
        }
        let cur = self.current.as_mut()?;
        if !cur.returned {
            cur.returned = true;
            self.serial += 1;
            self.shown += 1;
            if now - cur.ready.pts > self.frame_duration + 1e-9 {
                self.late += 1;
            }
        }
        let r = &cur.ready;
        Some(Shown { serial: self.serial, texture: r.texture.clone(), slice: r.slot as u32, pts: r.pts, left: r.left, top: r.top, width: r.width, height: r.height, bit_depth: r.bit_depth })
    }

    /// [`Player::frame_for`] at the clock's time.
    pub fn frame_now(&mut self) -> Option<Shown> {
        let t = self.clock.time();
        self.frame_for(t)
    }

    /// Jumps to movie time `t` (clamped to the movie): the clock moves there, and the picture
    /// whose time is the largest not after `t` becomes the first one in the queue.
    pub fn seek(&mut self, t: f64) {
        let t = t.clamp(self.start_time, self.end_time);
        let plan = plan_seek(&self.pts, &self.keys, (t * self.timescale + 1e-6).floor() as i64);
        self.epoch += 1;
        self.ended = false;
        self.failed = None;
        if let Some(c) = self.current.take() {
            self.release(c.ready.slot);
        }
        while let Some(r) = self.pending.pop_front() {
            self.release(r.slot);
        }
        let _ = self.cmd_tx.send(Cmd::Seek { epoch: self.epoch, start: plan.start_sample, skip_until: plan.landing_pts });
        self.clock.seek(t);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.lock().ok().and_then(|mut g| g.take()) {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_runs_pauses_seeks() {
        let t = Arc::new(FakeTime::new());
        let mut c = Clock::new(t.clone());
        assert_eq!(c.time(), 0.0);
        t.advance(5.0);
        assert_eq!(c.time(), 0.0); // paused
        c.start(10.0);
        t.advance(2.0);
        assert!((c.time() - 12.0).abs() < 1e-9);
        c.pause();
        t.advance(100.0);
        assert!((c.time() - 12.0).abs() < 1e-9);
        c.resume();
        t.advance(1.0);
        assert!((c.time() - 13.0).abs() < 1e-9);
        c.seek(3.0);
        t.advance(0.5);
        assert!((c.time() - 3.5).abs() < 1e-9);
    }

    #[test]
    fn seek_plan() {
        // decode order I P B B  I P B B (pts in ticks of 1; B frames before their P)
        let pts = [0, 3, 1, 2, 4, 7, 5, 6];
        let keys = [0, 4];
        // target 5.5: lands on pts 5 (sample 6), key at or before is 4, one back is 0
        assert_eq!(plan_seek(&pts, &keys, 5), SeekPlan { start_sample: 0, landing_pts: 5 });
        // target inside the first GOP: only one key
        assert_eq!(plan_seek(&pts, &keys, 2), SeekPlan { start_sample: 0, landing_pts: 2 });
        // before the start lands on the first picture
        assert_eq!(plan_seek(&pts, &keys, -10), SeekPlan { start_sample: 0, landing_pts: 0 });
        // past the end lands on the last
        assert_eq!(plan_seek(&pts, &keys, 100), SeekPlan { start_sample: 0, landing_pts: 7 });
        let keys3 = [0, 4, 8];
        let pts3 = [0, 3, 1, 2, 4, 7, 5, 6, 8, 11, 9, 10];
        assert_eq!(plan_seek(&pts3, &keys3, 9), SeekPlan { start_sample: 4, landing_pts: 9 });
    }
}
