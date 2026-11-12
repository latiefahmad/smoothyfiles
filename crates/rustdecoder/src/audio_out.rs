//! Audio output through WASAPI (shared mode, event driven) and the audio clock.
//!
//! [`AudioOutput`] opens the default render device in the device's own mix format, converts
//! what it is given (any channel count and rate: [`crate::resample`]) into that format, and a
//! render thread feeds the device from a queue whenever the device asks. It implements
//! [`AudioSink`], which is all [`crate::playback::Playback`] needs, so tests can swap in a fake.
//!
//! # The clock
//!
//! `IAudioClock::GetPosition` says how many frames the device has played since the stream was
//! last reset. Two things are corrected so the clock follows the sound that was actually heard:
//!
//! * frames the render thread wrote as silence because the queue ran dry (an under-run) are
//!   subtracted once they have played, so the clock stands still while the sound does, and the
//!   video waits instead of running ahead; silence after [`AudioSink::end_of_data`] is real
//!   time and counts;
//! * the position only moves in device periods (10 ms in shared mode), so it is interpolated
//!   with the QPC time `GetPosition` reports for it, never further than 20 ms, and never
//!   backwards.
//!
//! [`AudioClockSource`] (from [`AudioSink::time_source`]) is that clock as a [`TimeSource`]: seconds of
//! sound played since the output was opened, standing still when paused. A [`crate::player::Clock`]
//! over it is the A/V master clock.
//!
//! # Device changes
//!
//! An `IMMNotificationClient` tells the render thread when the default render device changes
//! (and `AUDCLNT_E_DEVICE_INVALIDATED`, `..._RESOURCES_INVALIDATED` and `..._SERVICE_NOT_RUNNING`
//! from any stream call do the same). The thread opens the new default device, then moves the
//! output over in one step ([`Inner::swap_device`]): the clock continues (what the old device
//! had been given and not played goes back to the front of the queue, so it is neither skipped
//! nor counted), the queued sound is converted if the new mix format differs, mute and volume
//! are applied to the new session, and a stream that was playing is started again. When no device
//! can be opened the clock stands still and the thread tries once a second.
//! [`AudioOutput::force_reopen`] does the same on demand (tests).
//!
//! # Silence
//!
//! `muted: true` does three things before the stream starts: sets the session's master volume
//! to 0, sets the session mute, and from then on hands every buffer to the device flagged
//! `AUDCLNT_BUFFERFLAGS_SILENT` (zeros), while still consuming the queue at the normal speed
//! so the clock runs as usual. Tests and examples use it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, PROPERTYKEY, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::{PCWSTR, implement};

use crate::player::TimeSource;
use crate::resample::{Resampler, map_channels};
use crate::{Error, Result};

/// What [`crate::playback::Playback`] needs from an audio output.
pub trait AudioSink: Send + Sync {
    /// The format of what [`AudioSink::push`] gets: interleaved `f32` at `rate`.
    fn set_input(&self, rate: u32, channels: usize);
    /// Queues audio that belongs at movie time `pts` seconds. The first push after a
    /// [`AudioSink::flush`] gives the sink its position; later pushes continue it.
    fn push(&self, pts: f64, pcm: &[f32]);
    /// No more audio until the next flush: the clock keeps running over the silence after the
    /// queue is empty.
    fn end_of_data(&self);
    /// Seconds of audio queued and not yet played.
    fn buffered(&self) -> f64;
    /// Throws away everything queued and played so far; the sink is paused afterwards.
    fn flush(&self);
    fn play(&self);
    fn pause(&self);
    /// Movie time of the sound being heard now; `None` before the first push.
    fn position(&self) -> Option<f64>;
    /// The clock as a time source (seconds of sound played, see the module comment).
    fn time_source(&self) -> Arc<dyn TimeSource>;
}

/// Options for [`AudioOutput::open`].
#[derive(Debug, Clone, Copy)]
pub struct OutputOptions {
    /// Nothing audible: session volume 0, session mute, silent buffers. See the module comment.
    pub muted: bool,
    /// Linear gain 0..1 applied to the samples (ignored while muted).
    pub volume: f32,
}

impl Default for OutputOptions {
    fn default() -> Self {
        // silent unless asked otherwise: nothing in tests makes a sound by accident
        OutputOptions { muted: true, volume: 1.0 }
    }
}

struct SendBox<T>(T);
// SAFETY: WASAPI interfaces and event handles are used from several threads of the same
// multithreaded apartment, which they allow.
unsafe impl<T> Send for SendBox<T> {}
unsafe impl<T> Sync for SendBox<T> {}

#[derive(Clone, Copy, PartialEq)]
enum SampleFormat {
    F32,
    I16,
}

struct Ring {
    /// Device-format samples (interleaved, `dev_channels` per frame) not yet handed over.
    data: VecDeque<f32>,
    /// The last quarter second of what was handed to the device (device format): a device change
    /// takes back what the old device had not played yet.
    history: VecDeque<f32>,
    eos: bool,
    /// Movie time of the first frame after the last flush.
    base_media: Option<f64>,
    /// Device frames written since the stream's last reset.
    written: u64,
    /// Silence the render thread wrote for lack of data: (first device frame, frames).
    silence: Vec<(u64, u64)>,
    underruns: u64,
    /// Seconds of non-silent sound played before the last reset (or device change).
    cum: f64,
    /// The value of `cum` right after the last flush: the movie position counts from there.
    cum_flush: f64,
    /// Position the device reported right after the last reset, in seconds.
    pos_zero: f64,
    /// Greatest value returned by the clock since the last reset (it never goes back).
    last: f64,
    running: bool,
}

struct Converter {
    in_rate: u32,
    in_channels: usize,
    resampler: Option<Resampler>,
}

/// What belongs to the device stream in use; replaced as a whole when the device changes.
struct Dev {
    clock: SendBox<IAudioClock>,
    volume: SendBox<ISimpleAudioVolume>,
    freq: f64,
    rate: u32,
    channels: usize,
}

/// Lock order, wherever more than one is held: `conv`, `ring`, `dev`.
struct Inner {
    ring: Mutex<Ring>,
    conv: Mutex<Converter>,
    dev: Mutex<Dev>,
    gain: AtomicU32,
    muted: AtomicBool,
    qpc_freq: f64,
    stop: AtomicBool,
    started: AtomicU64,
    /// The render thread's wake-up event (commands, device changes, the end).
    wake: SendBox<HANDLE>,
    /// The device has to be opened again (default device changed, device lost, or asked for).
    reopen: AtomicBool,
    /// Times the stream was moved to a new device stream.
    reopens: AtomicU64,
}

impl Inner {
    fn qpc_100ns(&self) -> f64 {
        let mut c = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut c);
        }
        c as f64 * 1e7 / self.qpc_freq
    }

    /// Device position in seconds since the stream was opened, interpolated; `last` (same
    /// scale) when the device does not answer.
    fn device_secs(&self, d: &Dev, running: bool, last: f64) -> f64 {
        let (mut p, mut q) = (0u64, 0u64);
        if unsafe { d.clock.0.GetPosition(&mut p, Some(&mut q)) }.is_err() {
            return last;
        }
        let mut s = p as f64 / d.freq;
        if running {
            let dt = ((self.qpc_100ns() - q as f64) * 1e-7).clamp(0.0, 0.02);
            s += dt;
        }
        s
    }

    /// Seconds of sound (silence from under-runs excluded) played in total.
    fn played(&self) -> f64 {
        let mut r = self.ring.lock().unwrap();
        let d = self.dev.lock().unwrap();
        let s = (self.device_secs(&d, r.running, r.pos_zero + r.last) - r.pos_zero).max(0.0);
        let frames = s * d.rate as f64;
        let silent: f64 = r.silence.iter().map(|&(a, n)| (frames - a as f64).clamp(0.0, n as f64)).sum();
        let t = r.cum + (frames - silent) / d.rate as f64;
        let t = t.max(r.last_total());
        r.last = t - r.cum;
        t
    }

    /// Moves the clock, the queue and the volume over to a freshly opened stream. The clock
    /// goes on from where it was: what the old device had been given but not played goes back
    /// to the front of the queue, so nothing is skipped. The queued sound is converted when the
    /// new device's rate or channel count differs.
    fn swap_device(&self, new: &Stream) {
        let mut c = self.conv.lock().unwrap();
        let mut r = self.ring.lock().unwrap();
        let mut d = self.dev.lock().unwrap();
        let secs = (self.device_secs(&d, r.running, r.pos_zero + r.last) - r.pos_zero).max(0.0);
        let frames = secs * d.rate as f64;
        let silent_total: f64 = r.silence.iter().map(|&(_, n)| n as f64).sum();
        let silent_played: f64 = r.silence.iter().map(|&(a, n)| (frames - a as f64).clamp(0.0, n as f64)).sum();
        let heard = frames - silent_played;
        let lost = (r.written as f64 - silent_total - heard).max(0.0);
        let played = (r.cum + heard / d.rate as f64).max(r.last_total());
        // what the old device was given and did not play comes back to the front of the queue
        // (still in the old format), so nothing is skipped and the clock does not jump
        let back = ((lost.round() as usize) * d.channels).min(r.history.len());
        let tail: Vec<f32> = r.history.iter().skip(r.history.len() - back).copied().collect();
        for s in tail.into_iter().rev() {
            r.data.push_front(s);
        }
        r.history.clear();
        r.cum = played;
        r.last = 0.0;
        r.silence.clear();
        r.written = 0;
        r.running = false;
        let mut p0 = 0u64;
        let _ = unsafe { new.clock.GetPosition(&mut p0, None) };
        r.pos_zero = p0 as f64 / new.freq;
        if (new.rate, new.channels) != (d.rate, d.channels) {
            let old: Vec<f32> = r.data.drain(..).collect();
            let mut mapped = Vec::new();
            map_channels(&old, d.channels, new.channels, &mut mapped);
            let mut out = Vec::new();
            if new.rate != d.rate {
                let mut rs = Resampler::new(d.rate, new.rate, new.channels);
                rs.process(&mapped, &mut out);
                rs.finish(&mut out);
            } else {
                out = mapped;
            }
            r.data = out.into();
        }
        c.resampler = (c.in_rate != new.rate).then(|| Resampler::new(c.in_rate, new.rate, new.channels));
        *d = Dev { clock: SendBox(new.clock.clone()), volume: SendBox(new.volume.clone()), freq: new.freq, rate: new.rate, channels: new.channels };
    }

    fn apply_mute(&self, volume: &ISimpleAudioVolume) {
        let muted = self.muted.load(Ordering::SeqCst);
        unsafe {
            if muted {
                let _ = volume.SetMasterVolume(0.0, std::ptr::null());
                let _ = volume.SetMute(true, std::ptr::null());
            } else {
                let _ = volume.SetMute(false, std::ptr::null());
                let _ = volume.SetMasterVolume(1.0, std::ptr::null());
            }
        }
    }

    fn request_reopen(&self) {
        self.reopen.store(true, Ordering::SeqCst);
        unsafe {
            let _ = SetEvent(self.wake.0);
        }
    }
}

impl Ring {
    fn last_total(&self) -> f64 {
        self.cum + self.last
    }
}

/// The audio clock as a time source.
pub struct AudioClockSource(Arc<Inner>);

impl TimeSource for AudioClockSource {
    fn now(&self) -> f64 {
        self.0.played()
    }
}

enum Cmd {
    Play(Sender<()>),
    Pause(Sender<()>),
    Flush(Sender<()>),
    Quit,
}

/// WASAPI shared-mode output.
pub struct AudioOutput {
    inner: Arc<Inner>,
    cmd: Sender<Cmd>,
    thread: Mutex<Option<JoinHandle<()>>>,
    device_name: String,
}

impl AudioOutput {
    /// Opens the default render device. Fails when there is none. From then on the output
    /// follows the default device: when it changes (or the device is lost) the stream is opened
    /// again on the new one, with volume, mute, queued sound and clock carried over.
    pub fn open(opts: OutputOptions) -> Result<AudioOutput> {
        let (init_tx, init_rx) = channel::<std::result::Result<String, String>>();
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let inner_slot: Arc<Mutex<Option<Arc<Inner>>>> = Arc::new(Mutex::new(None));
        let slot = inner_slot.clone();
        let thread = std::thread::Builder::new()
            .name("rustdecoder-audio-out".into())
            .spawn(move || render_thread(opts, init_tx, cmd_rx, slot))
            .map_err(Error::Io)?;
        let name = match init_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(Error::Gpu(format!("audio output: {e}")));
            }
            Err(_) => return Err(Error::Gpu("audio output: no answer from the device".into())),
        };
        let inner = inner_slot.lock().unwrap().take().ok_or(Error::Gpu("audio output: not initialised".into()))?;
        Ok(AudioOutput { inner, cmd: cmd_tx, thread: Mutex::new(Some(thread)), device_name: name })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The device's mix format: rate and channels (of the stream in use now).
    pub fn device_format(&self) -> (u32, usize) {
        let d = self.inner.dev.lock().unwrap();
        (d.rate, d.channels)
    }

    pub fn set_volume(&self, v: f32) {
        self.inner.gain.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Switches the silence of [`OutputOptions::muted`] on or off.
    pub fn set_muted(&self, muted: bool) {
        self.inner.muted.store(muted, Ordering::SeqCst);
        let d = self.inner.dev.lock().unwrap();
        self.inner.apply_mute(&d.volume.0);
    }

    /// Times the render thread had to write silence because the queue was empty.
    pub fn underruns(&self) -> u64 {
        self.inner.ring.lock().unwrap().underruns
    }

    /// Times the render thread has been woken by the device (shows the stream is alive).
    pub fn device_periods(&self) -> u64 {
        self.inner.started.load(Ordering::Relaxed)
    }

    /// Times the stream moved to a newly opened device stream (a default device change, a lost
    /// device, or [`AudioOutput::force_reopen`]).
    pub fn reopens(&self) -> u64 {
        self.inner.reopens.load(Ordering::SeqCst)
    }

    /// Does what a default device change or `AUDCLNT_E_DEVICE_INVALIDATED` makes the output do:
    /// opens the default device again and moves over to it (a test hook: the real triggers need a
    /// second device). Waits up to three seconds; `true` when the new stream is running.
    pub fn force_reopen(&self) -> bool {
        let before = self.reopens();
        self.inner.request_reopen();
        let t0 = std::time::Instant::now();
        while self.reopens() == before {
            if t0.elapsed() > Duration::from_secs(3) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    fn send(&self, make: impl FnOnce(Sender<()>) -> Cmd) {
        let (tx, rx) = channel();
        if self.cmd.send(make(tx)).is_ok() {
            unsafe {
                let _ = SetEvent(self.inner.wake.0);
            }
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        let _ = self.cmd.send(Cmd::Quit);
        unsafe {
            let _ = SetEvent(self.inner.wake.0);
        }
        if let Some(h) = self.thread.lock().ok().and_then(|mut g| g.take()) {
            let _ = h.join();
        }
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.inner.wake.0);
        }
    }
}

impl AudioSink for AudioOutput {
    fn set_input(&self, rate: u32, channels: usize) {
        let mut c = self.inner.conv.lock().unwrap();
        let (dev_rate, dev_channels) = {
            let d = self.inner.dev.lock().unwrap();
            (d.rate, d.channels)
        };
        c.in_rate = rate;
        c.in_channels = channels.max(1);
        c.resampler = (rate != dev_rate).then(|| Resampler::new(rate, dev_rate, dev_channels));
    }

    fn push(&self, pts: f64, pcm: &[f32]) {
        let mut mapped = Vec::new();
        let mut out = Vec::new();
        {
            let mut c = self.inner.conv.lock().unwrap();
            let dev_channels = self.inner.dev.lock().unwrap().channels;
            map_channels(pcm, c.in_channels, dev_channels, &mut mapped);
            match c.resampler.as_mut() {
                Some(r) => r.process(&mapped, &mut out),
                None => out = mapped,
            }
            // still under `conv`: a device change waits for it, so `out` is in the format the
            // queue holds
            let mut r = self.inner.ring.lock().unwrap();
            r.base_media.get_or_insert(pts);
            r.data.extend(out);
        }
    }

    fn end_of_data(&self) {
        // the resampler holds back the last few input frames: give them out
        let mut c = self.inner.conv.lock().unwrap();
        let mut out = Vec::new();
        if let Some(rs) = c.resampler.as_mut() {
            rs.finish(&mut out);
        }
        let mut r = self.inner.ring.lock().unwrap();
        r.data.extend(out);
        r.eos = true;
    }

    fn buffered(&self) -> f64 {
        let r = self.inner.ring.lock().unwrap();
        let d = self.inner.dev.lock().unwrap();
        (r.data.len() / d.channels) as f64 / d.rate as f64
    }

    fn flush(&self) {
        if let Some(rs) = self.inner.conv.lock().unwrap().resampler.as_mut() {
            rs.reset();
        }
        self.send(Cmd::Flush);
    }

    fn play(&self) {
        self.send(Cmd::Play);
    }

    fn pause(&self) {
        self.send(Cmd::Pause);
    }

    fn position(&self) -> Option<f64> {
        let base = self.inner.ring.lock().unwrap().base_media?;
        // `played` counts sound from the output's start; the movie position counts from the
        // last flush (`cum_flush`)
        let played = self.inner.played();
        let at_flush = self.inner.ring.lock().unwrap().cum_flush;
        Some(base + (played - at_flush).max(0.0))
    }

    fn time_source(&self) -> Arc<dyn TimeSource> {
        Arc::new(AudioClockSource(self.inner.clone()))
    }
}

// ---------------------------------------------------------------------------------------------
// the device

/// Told by Windows when the default render device changes: asks the render thread to reopen.
#[implement(IMMNotificationClient)]
struct DeviceWatcher {
    inner: Arc<Inner>,
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceWatcher_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            self.inner.request_reopen();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// An opened stream on a device.
struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    volume: ISimpleAudioVolume,
    event: HANDLE,
    size: u32,
    rate: u32,
    channels: usize,
    format: SampleFormat,
    freq: f64,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

fn fail<T>(what: &str, e: windows::core::Error) -> std::result::Result<T, String> {
    Err(format!("{what}: {e}"))
}

/// Opens the default render device in its mix format, event driven.
fn open_stream(en: &IMMDeviceEnumerator) -> std::result::Result<Stream, String> {
    unsafe {
        let dev = match en.GetDefaultAudioEndpoint(eRender, eConsole) {
            Ok(d) => d,
            Err(e) => return fail("no default output device", e),
        };
        let client: IAudioClient = match dev.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => return fail("activate", e),
        };
        let mix = match client.GetMixFormat() {
            Ok(m) => m,
            Err(e) => return fail("mix format", e),
        };
        let wf = *mix;
        let tag = wf.wFormatTag;
        let bits = wf.wBitsPerSample;
        let format = if tag == 3 || (tag == 0xFFFE && {
            let ext = &*(mix as *const WAVEFORMATEXTENSIBLE);
            ext.SubFormat.data1 == 3
        }) {
            (bits == 32).then_some(SampleFormat::F32)
        } else if tag == 1 || tag == 0xFFFE {
            (bits == 16).then_some(SampleFormat::I16)
        } else {
            None
        };
        let Some(format) = format else {
            CoTaskMemFree(Some(mix as *const _));
            return Err(format!("mix format not supported (tag {tag:#x}, {bits} bits)"));
        };
        let (rate, channels) = (wf.nSamplesPerSec, wf.nChannels as usize);
        if let Err(e) = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 1_000_000, 0, mix, None) {
            CoTaskMemFree(Some(mix as *const _));
            return fail("initialize", e);
        }
        CoTaskMemFree(Some(mix as *const _));
        let event = match CreateEventW(None, false, false, None) {
            Ok(h) => h,
            Err(e) => return fail("event", e),
        };
        // from here on `Stream`'s drop closes the event
        let make = || -> std::result::Result<Stream, String> {
            if let Err(e) = client.SetEventHandle(event) {
                return fail("event handle", e);
            }
            let size = match client.GetBufferSize() {
                Ok(s) => s,
                Err(e) => return fail("buffer size", e),
            };
            let render: IAudioRenderClient = match client.GetService() {
                Ok(r) => r,
                Err(e) => return fail("render client", e),
            };
            let clock: IAudioClock = match client.GetService() {
                Ok(r) => r,
                Err(e) => return fail("clock", e),
            };
            let volume: ISimpleAudioVolume = match client.GetService() {
                Ok(r) => r,
                Err(e) => return fail("session volume", e),
            };
            let freq = clock.GetFrequency().map_err(|e| format!("clock frequency: {e}"))? as f64;
            Ok(Stream { client: client.clone(), render, clock, volume, event, size, rate, channels, format, freq })
        };
        match make() {
            Ok(s) => Ok(s),
            Err(e) => {
                let _ = windows::Win32::Foundation::CloseHandle(event);
                Err(e)
            }
        }
    }
}

/// Whether a WASAPI call failed because the device (or its resources) went away.
fn device_lost(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == AUDCLNT_E_DEVICE_INVALIDATED || c == AUDCLNT_E_RESOURCES_INVALIDATED || c == AUDCLNT_E_SERVICE_NOT_RUNNING
}

/// Hands the device up to `max_frames` of what is queued (silence for the rest). `false` when the
/// device is lost.
fn fill(inner: &Inner, st: &Stream, max_frames: u32) -> bool {
    if max_frames == 0 {
        return true;
    }
    let channels = st.channels;
    unsafe {
        let p = match st.render.GetBuffer(max_frames) {
            Ok(p) => p,
            Err(e) => return !device_lost(&e),
        };
        let n = max_frames as usize;
        let muted = inner.muted.load(Ordering::Relaxed);
        let gain = f32::from_bits(inner.gain.load(Ordering::Relaxed));
        let mut flags = 0u32;
        let mut r = inner.ring.lock().unwrap();
        let have = (r.data.len() / channels).min(n);
        let short = n - have;
        let mut tmp = Vec::with_capacity(have * channels);
        tmp.extend(r.data.drain(..have * channels));
        r.history.extend(tmp.iter().copied());
        let keep = st.rate as usize * channels / 4;
        if r.history.len() > keep {
            let extra = r.history.len() - keep;
            r.history.drain(..extra);
        }
        if short > 0 && !r.eos {
            let start = r.written + have as u64;
            r.silence.push((start, short as u64));
            r.underruns += 1;
        }
        r.written += n as u64;
        drop(r);
        if muted || have == 0 {
            flags |= AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
        }
        match st.format {
            SampleFormat::F32 => {
                let out = std::slice::from_raw_parts_mut(p as *mut f32, n * channels);
                if flags != 0 {
                    out.fill(0.0);
                } else {
                    for (o, s) in out.iter_mut().zip(&tmp) {
                        *o = (s * gain).clamp(-1.0, 1.0);
                    }
                    out[tmp.len()..].fill(0.0);
                }
            }
            SampleFormat::I16 => {
                let out = std::slice::from_raw_parts_mut(p as *mut i16, n * channels);
                if flags != 0 {
                    out.fill(0);
                } else {
                    for (o, s) in out.iter_mut().zip(&tmp) {
                        *o = ((s * gain).clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    }
                    out[tmp.len()..].fill(0);
                }
            }
        }
        match st.render.ReleaseBuffer(max_frames, flags) {
            Ok(()) => true,
            Err(e) => !device_lost(&e),
        }
    }
}

/// Tops the device buffer up. `false` when the device is lost.
fn top_up(inner: &Inner, st: &Stream) -> bool {
    match unsafe { st.client.GetCurrentPadding() } {
        Ok(pad) => fill(inner, st, st.size.saturating_sub(pad)),
        Err(e) => !device_lost(&e),
    }
}

// ---------------------------------------------------------------------------------------------
// the render thread

fn render_thread(
    opts: OutputOptions,
    init_tx: Sender<std::result::Result<String, String>>,
    cmd_rx: Receiver<Cmd>,
    slot: Arc<Mutex<Option<Arc<Inner>>>>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let en: IMMDeviceEnumerator = match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
        Ok(e) => e,
        Err(e) => {
            let _ = init_tx.send(Err(format!("device enumerator: {e}")));
            return;
        }
    };
    let first = match open_stream(&en) {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    let wake = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            let _ = init_tx.send(Err(format!("event: {e}")));
            return;
        }
    };
    let mut qf = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut qf);
    }
    let inner = Arc::new(Inner {
        ring: Mutex::new(Ring {
            data: VecDeque::new(),
            history: VecDeque::new(),
            eos: false,
            base_media: None,
            written: 0,
            silence: Vec::new(),
            underruns: 0,
            cum: 0.0,
            cum_flush: 0.0,
            pos_zero: 0.0,
            last: 0.0,
            running: false,
        }),
        conv: Mutex::new(Converter { in_rate: first.rate, in_channels: first.channels, resampler: None }),
        dev: Mutex::new(Dev { clock: SendBox(first.clock.clone()), volume: SendBox(first.volume.clone()), freq: first.freq, rate: first.rate, channels: first.channels }),
        gain: AtomicU32::new(opts.volume.clamp(0.0, 1.0).to_bits()),
        muted: AtomicBool::new(opts.muted),
        qpc_freq: qf.max(1) as f64,
        stop: AtomicBool::new(false),
        started: AtomicU64::new(0),
        wake: SendBox(wake),
        reopen: AtomicBool::new(false),
        reopens: AtomicU64::new(0),
    });
    // silence first: before anything can be played
    inner.apply_mute(&first.volume);
    // follow the default device
    let watcher: IMMNotificationClient = DeviceWatcher { inner: inner.clone() }.into();
    let watching = unsafe { en.RegisterEndpointNotificationCallback(&watcher) }.is_ok();
    *slot.lock().unwrap() = Some(inner.clone());
    let _ = init_tx.send(Ok(String::from("default render device")));

    // `wake` is closed by `AudioOutput::drop` after the join (it still signals it on the way out)
    let mut stream = Some(first);
    // the caller wants sound flowing (a stream is started whenever there is one)
    let mut running = false;
    let mut retry_at = std::time::Instant::now();
    loop {
        // woken by the device (buffer space) or by a command
        let timeout = match (&stream, running) {
            (None, _) => 250,
            (Some(_), true) => 500,
            (Some(_), false) => INFINITE,
        };
        let handles = [stream.as_ref().map_or(wake, |s| s.event), wake];
        let wait = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        if inner.stop.load(Ordering::SeqCst) {
            break;
        }
        while let Ok(c) = cmd_rx.try_recv() {
            match c {
                Cmd::Quit => {
                    if watching {
                        unsafe {
                            let _ = en.UnregisterEndpointNotificationCallback(&watcher);
                        }
                    }
                    return;
                }
                Cmd::Pause(ack) => {
                    if running {
                        if let Some(st) = &stream {
                            unsafe {
                                let _ = st.client.Stop();
                            }
                        }
                        running = false;
                        inner.ring.lock().unwrap().running = false;
                    }
                    let _ = ack.send(());
                }
                Cmd::Play(ack) => {
                    if !running {
                        running = true;
                        if let Some(st) = &stream {
                            start(&inner, st);
                        }
                    }
                    let _ = ack.send(());
                }
                Cmd::Flush(ack) => {
                    if let Some(st) = &stream {
                        flush(&inner, st, &mut running);
                    } else {
                        let mut r = inner.ring.lock().unwrap();
                        r.cum_flush = r.cum;
                        r.data.clear();
                        r.history.clear();
                        r.eos = false;
                        r.base_media = None;
                        r.running = false;
                        r.silence.clear();
                        r.written = 0;
                        r.last = 0.0;
                        running = false;
                    }
                    let _ = ack.send(());
                }
            }
        }
        // the device went away, the default changed, or somebody asked: open again
        let mut lost = inner.reopen.swap(false, Ordering::SeqCst);
        if stream.is_none() && std::time::Instant::now() >= retry_at {
            lost = true;
        }
        if lost {
            match open_stream(&en) {
                Ok(new) => {
                    inner.swap_device(&new);
                    inner.apply_mute(&new.volume);
                    // the old stream goes (stopped) only now, so the new one is already set up
                    stream = Some(new);
                    if running {
                        if let Some(st) = &stream {
                            start(&inner, st);
                        }
                    }
                    inner.reopens.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) => {
                    log::warn!("audio output: reopening the device failed: {e}");
                    // nothing plays until a device comes back: the clock stands still
                    if let Some(st) = stream.take() {
                        drop(st);
                    }
                    inner.ring.lock().unwrap().running = false;
                    retry_at = std::time::Instant::now() + Duration::from_secs(1);
                }
            }
            continue;
        }
        if let Some(st) = &stream {
            if running {
                if wait == WAIT_OBJECT_0 {
                    inner.started.fetch_add(1, Ordering::Relaxed);
                }
                // woken by the device, or by the timeout: top up either way
                if !top_up(&inner, st) {
                    inner.request_reopen();
                }
            }
        }
    }
    if watching {
        unsafe {
            let _ = en.UnregisterEndpointNotificationCallback(&watcher);
        }
    }
}

/// Fills the device buffer before the stream starts, so it never starts empty, and starts it.
fn start(inner: &Inner, st: &Stream) {
    let pad = unsafe { st.client.GetCurrentPadding() }.unwrap_or(0);
    fill(inner, st, st.size.saturating_sub(pad));
    inner.ring.lock().unwrap().running = true;
    unsafe {
        let _ = st.client.Start();
    }
}

/// Stops the stream and throws away what was queued and played: the clock restarts its count.
fn flush(inner: &Inner, st: &Stream, running: &mut bool) {
    unsafe {
        if *running {
            let _ = st.client.Stop();
        }
        *running = false;
        let (mut before, mut after) = (0u64, 0u64);
        let _ = st.clock.GetPosition(&mut before, None);
        let _ = st.client.Reset();
        let _ = st.clock.GetPosition(&mut after, None);
        let mut r = inner.ring.lock().unwrap();
        let (freq, rate) = (st.freq, st.rate as f64);
        // sound played so far, with the silence taken out
        let b = (before as f64 / freq - r.pos_zero).max(0.0) * rate;
        let silent: f64 = r.silence.iter().map(|&(a, n)| (b - a as f64).clamp(0.0, n as f64)).sum();
        r.cum += (b - silent) / rate;
        r.cum_flush = r.cum;
        r.pos_zero = after as f64 / freq;
        r.silence.clear();
        r.written = 0;
        r.last = 0.0;
        r.data.clear();
        r.history.clear();
        r.eos = false;
        r.base_media = None;
        r.running = false;
    }
}
