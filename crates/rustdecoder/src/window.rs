//! Playing into a window: [`VideoWindow`] owns a [`Playback`] and presents its pictures into an
//! HWND through a flip-model swap chain on the decoder's own D3D11 device, so the picture never
//! leaves the GPU.
//!
//! ```text
//!   decode thread ─► ring ─► Playback::frame() ─┐
//!   audio feed ─► WASAPI ─► audio clock ────────┤   render thread (this module)
//!                                               ▼
//!      wait for the swap chain's latency object ─► Converter::convert_rect into the back
//!      buffer (letterboxed, background colour around) ─► Present(1)
//! ```
//!
//! * The swap chain is `FLIP_DISCARD`, BGRA8, three buffers, with the frame-latency waitable
//!   object (latency 1): the render thread waits on it right before it draws, so a late frame
//!   never queues behind an older one.
//! * A frame is drawn and presented only when the picture changed, the window size changed, or
//!   a capture was asked for. A paused window presents nothing and the thread only wakes every
//!   few milliseconds to look for commands.
//! * The window size is read from the client rectangle every wake-up, so any HWND works,
//!   whoever created it and whatever way it is resized.
//! * [`Placement`] says where the window comes from: a top-level window, a popup owned by
//!   another window (placed in screen coordinates; this is how the app puts it over a pane: an
//!   owned window does not attach two threads' input queues the way a child window does), a
//!   child window, or an HWND somebody else made. The first three are created on a thread of
//!   their own with its own message pump, so the caller's thread is never involved.
//! * Everything the caller does ([`VideoWindow::play`], [`VideoWindow::seek`], …) is a message
//!   to the render thread, which owns the `Playback`; [`VideoWindow::state`] is a snapshot it
//!   keeps up to date. Nothing blocks the caller's thread.
//!
//! [`VideoWindow::capture`] reads the back buffer (just before the present that follows) for
//! tests: with the flip model, `PrintWindow` and GDI captures do not see the picture.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

use crate::player::Shown;
use crate::playback::{Playback, PlaybackOptions};
use crate::present::{Colorimetry, Converter, Params, Target, rotated_size};
use crate::{Error, Result};

const BUFFERS: u32 = 3;
const FLAGS: i32 = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;

/// Where the video is shown.
#[derive(Debug, Clone)]
pub enum Placement {
    /// A window of its own with a title bar; `width` x `height` is the client size.
    TopLevel { title: String, width: u32, height: u32 },
    /// A borderless popup owned by `owner` (an HWND as `isize`), at screen coordinates.
    Popup { owner: isize, x: i32, y: i32, width: u32, height: u32 },
    /// A child window of `parent`, at client coordinates of the parent.
    Child { parent: isize, x: i32, y: i32, width: u32, height: u32 },
    /// A window made by the caller (an HWND as `isize`); it must outlive the [`VideoWindow`].
    Existing(isize),
}

#[derive(Debug, Clone)]
pub struct WindowOptions {
    /// Colour around the picture (red, green, blue, alpha; 0..1).
    pub background: [f32; 4],
    /// Play the sound (a device output is opened when the clip has a track).
    pub audio: bool,
    pub muted: bool,
    pub volume: f32,
    /// Start again at the beginning when the end is reached.
    pub looping: bool,
    /// Start playing as soon as the window is up.
    pub autoplay: bool,
    /// Created shown (not activated) or hidden.
    pub visible: bool,
    /// Pictures decoded ahead.
    pub ring: usize,
}

impl Default for WindowOptions {
    fn default() -> Self {
        WindowOptions { background: [0.0, 0.0, 0.0, 1.0], audio: true, muted: false, volume: 1.0, looping: false, autoplay: true, visible: true, ring: 6 }
    }
}

/// What the render thread knows, for the UI.
#[derive(Debug, Clone, Default)]
pub struct WindowState {
    pub playing: bool,
    /// The end was reached (and not looping); playing again starts from the beginning.
    pub ended: bool,
    /// Seconds from the start of the movie.
    pub position: f64,
    pub duration: f64,
    /// Presentation time (seconds from the start) of the picture on show, once there is one.
    pub picture: Option<f64>,
    /// Seconds of the sound being heard (from the start), when there is audio.
    pub audio_position: Option<f64>,
    pub has_audio: bool,
    pub audio_note: String,
    /// Swap chain presents so far.
    pub presented: u64,
    /// Pictures handed out / passed over / shown late (see [`crate::player::Stats`]).
    pub shown: u64,
    pub dropped: u64,
    pub late: u64,
    /// Back buffer size.
    pub size: (u32, u32),
    /// Size of the picture as shown (after the pixel aspect ratio and the rotation).
    pub video_size: (u32, u32),
    pub codec: &'static str,
    pub volume: f32,
    pub muted: bool,
    /// Why the video stopped working, if it did.
    pub error: Option<String>,
    /// The window is gone (closed by the user).
    pub closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    /// Left button went down on the picture.
    Click,
    DoubleClick,
    /// The wheel turned over the picture: `120` per notch away from the user (up), negative
    /// towards.
    Wheel(i32),
    /// The pointer moved over the picture (several moves in a row are one event).
    MouseMove,
    /// The pointer left the window.
    Leave,
    /// The user closed the window.
    Closed,
}

/// The back buffer as read back: BGRA, rows tightly packed.
#[derive(Debug, Clone)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Capture {
    /// `[r, g, b, a]` at a pixel.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.bgra[i + 2], self.bgra[i + 1], self.bgra[i], self.bgra[i + 3]]
    }

    /// Mean `[r, g, b]` over a rectangle.
    pub fn mean(&self, x: u32, y: u32, w: u32, h: u32) -> [f64; 3] {
        let mut sum = [0u64; 3];
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let p = self.pixel(xx, yy);
                for k in 0..3 {
                    sum[k] += p[k] as u64;
                }
            }
        }
        let n = (w.min(self.width - x.min(self.width)) * h.min(self.height - y.min(self.height))).max(1) as f64;
        [sum[0] as f64 / n, sum[1] as f64 / n, sum[2] as f64 / n]
    }
}

/// The rectangle `(x, y, w, h)` a `vw` x `vh` picture takes in a `cw` x `ch` window: as large as
/// fits with the aspect ratio kept, centred.
pub fn letterbox(cw: u32, ch: u32, vw: u32, vh: u32) -> (i32, i32, u32, u32) {
    if cw == 0 || ch == 0 || vw == 0 || vh == 0 {
        return (0, 0, cw, ch);
    }
    let s = (cw as f64 / vw as f64).min(ch as f64 / vh as f64);
    let w = ((vw as f64 * s).round() as u32).clamp(1, cw);
    let h = ((vh as f64 * s).round() as u32).clamp(1, ch);
    (((cw - w) / 2) as i32, ((ch - h) / 2) as i32, w, h)
}

/// What a `frame` is shown as, before the rotation: its size, the width scaled by the stream's
/// pixel aspect ratio (`display` against `coded`, from [`crate::decoder::StreamInfo`]).
pub fn shown_size(frame: (u32, u32), coded: (u32, u32), display: (u32, u32)) -> (u32, u32) {
    if frame == coded || coded.0 == 0 || coded.1 == 0 || display.0 == 0 || display.1 == 0 {
        return if frame == coded && display.0 > 0 && display.1 > 0 { display } else { frame };
    }
    let par = (display.0 as f64 * coded.1 as f64) / (display.1 as f64 * coded.0 as f64);
    (((frame.0 as f64 * par).round() as u32).max(1), frame.1)
}

enum Cmd {
    Play,
    Pause,
    /// Play if paused, pause if playing: decided by the render thread, which knows. (Deciding
    /// from the published state sent a second Play while the first was still starting, and the
    /// pause was lost.)
    Toggle,
    /// Seconds from the start.
    Seek(f64),
    Volume(f32),
    Mute(bool),
    Looping(bool),
    Background([f32; 4]),
    Redraw,
    Capture(Sender<std::result::Result<Capture, String>>),
}

struct Shared {
    state: Mutex<WindowState>,
    events: Mutex<VecDeque<WindowEvent>>,
    stop: AtomicBool,
    closed: AtomicBool,
    hwnd: AtomicU64,
}

/// A playing video in a window. Dropping it stops the playback and destroys the window it made.
pub struct VideoWindow {
    shared: Arc<Shared>,
    cmd: Sender<Cmd>,
    render: Option<JoinHandle<()>>,
    window: Option<JoinHandle<()>>,
    hwnd: isize,
    owns_window: bool,
}

// ---------------------------------------------------------------------------------------------
// the window

thread_local! {
    static SHARED: std::cell::RefCell<Option<Arc<Shared>>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let push = |e: WindowEvent| SHARED.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            let mut q = s.events.lock().unwrap();
            // moves arrive by the hundred: one in the queue says it all
            if e == WindowEvent::MouseMove && q.back() == Some(&e) {
                return;
            }
            q.push_back(e);
        }
    });
    match msg {
        WM_MOUSEMOVE => {
            // ask for WM_MOUSELEAVE (the request ends with every leave)
            let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            unsafe {
                let _ = TrackMouseEvent(&mut t);
            }
            push(WindowEvent::MouseMove);
        }
        WM_MOUSELEAVE => push(WindowEvent::Leave),
        WM_ERASEBKGND => return LRESULT(1),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => push(WindowEvent::Click),
        WM_LBUTTONDBLCLK => push(WindowEvent::DoubleClick),
        WM_MOUSEWHEEL => push(WindowEvent::Wheel(((wp.0 >> 16) & 0xffff) as u16 as i16 as i32)),
        WM_CLOSE => {
            push(WindowEvent::Closed);
            SHARED.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    s.closed.store(true, Ordering::SeqCst);
                }
            });
        }
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn register_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let Ok(inst) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: w!("rustdecoder.VideoWindow"),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn window_thread(place: Placement, visible: bool, shared: Arc<Shared>, tx: Sender<std::result::Result<isize, String>>) {
    SHARED.with(|s| *s.borrow_mut() = Some(shared));
    register_class();
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let hwnd = match &place {
            Placement::TopLevel { title, width, height } => {
                let style = WS_OVERLAPPEDWINDOW;
                let mut r = RECT { left: 0, top: 0, right: *width as i32, bottom: *height as i32 };
                let _ = AdjustWindowRectEx(&mut r, style, false, WINDOW_EX_STYLE(0));
                let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("rustdecoder.VideoWindow"),
                    windows::core::PCWSTR(t.as_ptr()),
                    style,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    r.right - r.left,
                    r.bottom - r.top,
                    None,
                    None,
                    Some(inst.into()),
                    None,
                )
            }
            Placement::Popup { owner, x, y, width, height } => CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
                w!("rustdecoder.VideoWindow"),
                w!(""),
                WS_POPUP,
                *x,
                *y,
                (*width).max(1) as i32,
                (*height).max(1) as i32,
                Some(HWND(*owner as *mut _)),
                None,
                Some(inst.into()),
                None,
            ),
            Placement::Child { parent, x, y, width, height } => CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("rustdecoder.VideoWindow"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS,
                *x,
                *y,
                (*width).max(1) as i32,
                (*height).max(1) as i32,
                Some(HWND(*parent as *mut _)),
                None,
                Some(inst.into()),
                None,
            ),
            Placement::Existing(_) => unreachable!("no window thread for an existing window"),
        };
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(format!("CreateWindowEx: {e}")));
                return;
            }
        };
        if visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let _ = tx.send(Ok(hwnd.0 as isize));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe {
        if GetClientRect(hwnd, &mut r).is_err() {
            return (0, 0);
        }
    }
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

// ---------------------------------------------------------------------------------------------
// the render thread

struct SendBox<T>(T);
// SAFETY: the Playback moves into the render thread and is used from there only; D3D11 objects
// are free threaded and the immediate context is multithread protected (see crate::player).
unsafe impl<T> Send for SendBox<T> {}

struct Swap {
    sc: IDXGISwapChain1,
    waitable: HANDLE,
    size: (u32, u32),
    target: Option<Target>,
}

impl Drop for Swap {
    fn drop(&mut self) {
        // the frame latency object is ours to close (DXGI hands out a new handle)
        if !self.waitable.is_invalid() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.waitable);
            }
        }
    }
}

fn gpu<T>(what: &str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|e| Error::Gpu(format!("{what}: {e}")))
}

fn make_swap(device: &ID3D11Device, hwnd: HWND, size: (u32, u32)) -> Result<Swap> {
    unsafe {
        let dxgi: IDXGIDevice = gpu("IDXGIDevice", device.cast())?;
        let adapter = gpu("GetAdapter", dxgi.GetAdapter())?;
        let factory: IDXGIFactory2 = gpu("IDXGIFactory2", adapter.GetParent())?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0.max(1),
            Height: size.1.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: FLAGS as u32,
        };
        let sc = gpu("CreateSwapChainForHwnd", factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None))?;
        let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
        let sc2: IDXGISwapChain2 = gpu("IDXGISwapChain2", sc.cast())?;
        gpu("SetMaximumFrameLatency", sc2.SetMaximumFrameLatency(1))?;
        let waitable = sc2.GetFrameLatencyWaitableObject();
        Ok(Swap { sc, waitable, size: (desc.Width, desc.Height), target: None })
    }
}

struct Render {
    pb: Playback,
    conv: Converter,
    swap: Swap,
    hwnd: HWND,
    shared: Arc<Shared>,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    params: Params,
    /// Coded size and display size of the stream (see [`shown_size`]).
    coded: (u32, u32),
    display: (u32, u32),
    background: [f32; 4],
    looping: bool,
    volume: f32,
    muted: bool,
    shown: Option<Shown>,
    drawn_serial: u64,
    dirty: bool,
    presented: u64,
    start: f64,
    end: f64,
    ended: bool,
    capture: Vec<Sender<std::result::Result<Capture, String>>>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    codec: &'static str,
}

impl Render {
    /// Applies `first` and whatever else is queued, in order, except that of several seeks only
    /// the last runs: a dragged timeline sends many, and each blocks until its picture is there.
    fn apply_queued(&mut self, first: Option<Cmd>, rx: &Receiver<Cmd>) {
        let mut queued: Vec<Cmd> = first.into_iter().collect();
        while let Ok(c) = rx.try_recv() {
            queued.push(c);
        }
        let last_seek = queued.iter().rposition(|c| matches!(c, Cmd::Seek(_)));
        for (i, c) in queued.into_iter().enumerate() {
            if matches!(c, Cmd::Seek(_)) && Some(i) != last_seek {
                continue;
            }
            self.apply(c);
        }
    }

    fn apply(&mut self, c: Cmd) {
        match c {
            Cmd::Toggle if self.pb.is_playing() => self.pb.pause(),
            Cmd::Play | Cmd::Toggle => {
                if self.ended {
                    self.pb.seek(self.start);
                    self.ended = false;
                }
                self.pb.play();
            }
            Cmd::Pause => self.pb.pause(),
            Cmd::Seek(t) => {
                self.pb.seek(self.start + t.max(0.0));
                self.ended = false;
            }
            Cmd::Volume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                self.pb.set_volume(self.volume);
            }
            Cmd::Mute(m) => {
                self.muted = m;
                self.pb.set_muted(m);
            }
            Cmd::Looping(l) => self.looping = l,
            Cmd::Background(b) => {
                self.background = b;
                self.dirty = true;
            }
            Cmd::Redraw => self.dirty = true,
            Cmd::Capture(tx) => {
                self.capture.push(tx);
                self.dirty = true;
            }
        }
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        // every reference to a back buffer goes before ResizeBuffers
        self.swap.target = None;
        unsafe {
            self.context.ClearState();
            self.context.Flush();
            gpu("ResizeBuffers", self.swap.sc.ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(FLAGS)))?;
        }
        self.swap.size = size;
        self.dirty = true;
        Ok(())
    }

    fn back_buffer(&mut self) -> Result<Target> {
        if let Some(t) = &self.swap.target {
            return Ok(t.clone());
        }
        let bb: ID3D11Texture2D = gpu("GetBuffer", unsafe { self.swap.sc.GetBuffer(0) })?;
        let t = self.conv.target_for(&bb)?;
        self.swap.target = Some(t.clone());
        Ok(t)
    }

    /// Draws the picture on show (or just the background) and presents.
    fn draw(&mut self) -> Result<()> {
        let target = self.back_buffer()?;
        // never ahead of the display: the swap chain says when it can take another frame
        unsafe {
            let _ = WaitForSingleObject(self.swap.waitable, 100);
        }
        self.conv.clear(&target, self.background);
        if let Some(f) = &self.shown {
            let (sw, sh) = shown_size((f.width, f.height), self.coded, self.display);
            let (vw, vh) = rotated_size(sw, sh, self.params.rotation);
            let r = letterbox(target.width, target.height, vw, vh);
            self.conv.convert_rect(&f.source(), &self.params, &target, r)?;
            self.drawn_serial = f.serial;
        }
        if !self.capture.is_empty() {
            let c = self.read_back(&target);
            for tx in self.capture.drain(..) {
                let _ = tx.send(c.clone());
            }
        }
        let hr = unsafe { self.swap.sc.Present(1, DXGI_PRESENT(0)) };
        if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
            return Err(Error::Gpu(format!("Present: {hr:?}")));
        }
        self.presented += 1;
        self.dirty = false;
        Ok(())
    }

    fn read_back(&mut self, t: &Target) -> std::result::Result<Capture, String> {
        unsafe {
            if self.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (t.width, t.height)) {
                let d = D3D11_TEXTURE2D_DESC {
                    Width: t.width,
                    Height: t.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                };
                let mut s = None;
                self.device.CreateTexture2D(&d, None, Some(&mut s)).map_err(|e| format!("staging: {e}"))?;
                self.staging = Some((s.ok_or("no staging texture")?, t.width, t.height));
            }
            let staging = &self.staging.as_ref().unwrap().0;
            self.context.CopyResource(staging, &t.texture);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| format!("Map: {e}"))?;
            let mut bgra = Vec::with_capacity((t.width * t.height * 4) as usize);
            for row in 0..t.height as usize {
                bgra.extend_from_slice(std::slice::from_raw_parts((m.pData as *const u8).add(row * m.RowPitch as usize), t.width as usize * 4));
            }
            self.context.Unmap(staging, 0);
            Ok(Capture { width: t.width, height: t.height, bgra })
        }
    }

    fn publish(&self, error: Option<&str>) {
        let time = self.pb.time();
        let st = self.pb.stats();
        let mut s = self.shared.state.lock().unwrap();
        s.playing = self.pb.is_playing();
        s.ended = self.ended;
        s.duration = (self.end - self.start).max(0.0);
        s.position = if self.ended { s.duration } else { (time - self.start).clamp(0.0, s.duration) };
        s.picture = self.shown.as_ref().map(|f| f.pts - self.start);
        s.audio_position = self.pb.audio_position().map(|p| p - self.start);
        s.has_audio = self.pb.has_audio();
        s.audio_note = self.pb.audio_note().to_string();
        s.presented = self.presented;
        (s.shown, s.dropped, s.late) = (st.shown, st.dropped, st.late);
        s.size = self.swap.size;
        s.video_size = self.shown.as_ref().map_or((0, 0), |f| {
            let (w, h) = shown_size((f.width, f.height), self.coded, self.display);
            rotated_size(w, h, self.params.rotation)
        });
        s.codec = self.codec;
        s.volume = self.volume;
        s.muted = self.muted;
        if let Some(e) = error.map(str::to_string).or_else(|| self.pb.player.error().map(str::to_string)) {
            s.error = Some(e);
        }
        s.closed = self.shared.closed.load(Ordering::SeqCst);
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        let mut error: Option<String> = None;
        let mut last_publish = Instant::now() - Duration::from_secs(1);
        while !self.shared.stop.load(Ordering::SeqCst) && error.is_none() {
            self.apply_queued(None, &rx);
            // size
            let (cw, ch) = client_size(self.hwnd);
            if cw > 0 && ch > 0 && (cw, ch) != self.swap.size {
                if let Err(e) = self.resize((cw, ch)) {
                    error = Some(e.to_string());
                    break;
                }
            }
            // end of the movie
            if self.pb.is_playing() && self.pb.finished() && self.pb.time() >= self.end - 1e-3 {
                if self.looping {
                    self.pb.seek(self.start);
                } else {
                    self.pb.pause();
                    self.ended = true;
                    self.dirty = true;
                }
            }
            // picture
            if let Some(f) = self.pb.frame() {
                if f.serial != self.drawn_serial || self.shown.is_none() {
                    self.shown = Some(f);
                    self.dirty = true;
                }
            }
            if self.dirty && cw > 0 && ch > 0 {
                if let Err(e) = self.draw() {
                    error = Some(e.to_string());
                    break;
                }
            } else if self.dirty && !self.capture.is_empty() {
                for tx in self.capture.drain(..) {
                    let _ = tx.send(Err("the window has no size".into()));
                }
                self.dirty = false;
            }
            if last_publish.elapsed() >= Duration::from_millis(15) || self.dirty {
                self.publish(None);
                last_publish = Instant::now();
            }
            // sleep until the next look; a command wakes it at once
            let wait = if self.pb.is_playing() { Duration::from_millis(1) } else { Duration::from_millis(10) };
            match rx.recv_timeout(wait) {
                Ok(c) => self.apply_queued(Some(c), &rx),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        for tx in self.capture.drain(..) {
            let _ = tx.send(Err("the window is closing".into()));
        }
        self.publish(error.as_deref());
    }
}

// ---------------------------------------------------------------------------------------------
// the handle

impl VideoWindow {
    /// Opens `path` and starts showing it in `place`. Blocks while the decoder and the audio
    /// device are set up (tens of milliseconds); call it off the UI thread.
    pub fn open(path: &Path, place: Placement, opts: WindowOptions) -> Result<VideoWindow> {
        let shared = Arc::new(Shared {
            state: Mutex::new(WindowState::default()),
            events: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            hwnd: AtomicU64::new(0),
        });
        let pb = Playback::open(path, PlaybackOptions { ring: opts.ring, audio: opts.audio, output: crate::audio_out::OutputOptions { muted: opts.muted, volume: opts.volume } })?;
        pb.set_volume(opts.volume);
        pb.set_muted(opts.muted);

        let (hwnd, window, owns_window) = match place {
            Placement::Existing(h) => (h, None, false),
            p => {
                let (tx, rx) = channel();
                let s2 = shared.clone();
                let visible = opts.visible;
                let t = std::thread::Builder::new().name("rustdecoder-window".into()).spawn(move || window_thread(p, visible, s2, tx)).map_err(Error::Io)?;
                match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(Ok(h)) => (h, Some(t), true),
                    Ok(Err(e)) => {
                        let _ = t.join();
                        return Err(Error::Gpu(e));
                    }
                    Err(_) => return Err(Error::Gpu("window thread did not answer".into())),
                }
            }
        };
        shared.hwnd.store(hwnd as u64, Ordering::SeqCst);
        let handle = HWND(hwnd as *mut _);

        let info = pb.player.info().clone();
        let (start, end) = pb.player.range();
        let device = pb.player.device().clone();
        let context = pb.player.context().clone();
        let mut size = client_size(handle);
        if size.0 == 0 || size.1 == 0 {
            size = (info.width.max(1), info.height.max(1));
        }
        let setup = || -> Result<(Converter, Swap)> {
            let conv = Converter::new(&device, &context)?;
            let swap = make_swap(&device, handle, size)?;
            Ok((conv, swap))
        };
        let (conv, swap) = match setup() {
            Ok(v) => v,
            Err(e) => {
                if owns_window {
                    unsafe {
                        let _ = PostMessageW(Some(handle), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    if let Some(w) = window {
                        let _ = w.join();
                    }
                }
                return Err(e);
            }
        };
        let params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: info.rotation, flip: false };
        let codec = pb.player.codec_name();
        let mut r = Render {
            pb,
            conv,
            swap,
            hwnd: handle,
            shared: shared.clone(),
            context,
            device,
            params,
            coded: (info.width, info.height),
            display: info.display,
            background: opts.background,
            looping: opts.looping,
            volume: opts.volume,
            muted: opts.muted,
            shown: None,
            drawn_serial: 0,
            dirty: true,
            presented: 0,
            start,
            end,
            ended: false,
            capture: Vec::new(),
            staging: None,
            codec,
        };
        // the first thing on screen is the background, not whatever the window had
        if let Err(e) = r.draw() {
            log::warn!("first present: {e}");
        }
        r.publish(None);
        let (cmd, rx) = channel();
        if opts.autoplay {
            let _ = cmd.send(Cmd::Play);
        }
        let boxed = SendBox(r);
        let render = std::thread::Builder::new()
            .name("rustdecoder-render".into())
            .spawn(move || {
                let b = boxed;
                b.0.run(rx);
            })
            .map_err(Error::Io)?;
        Ok(VideoWindow { shared, cmd, render: Some(render), window, hwnd, owns_window })
    }

    /// The window's HWND (as `isize`).
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn play(&self) {
        let _ = self.cmd.send(Cmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd.send(Cmd::Pause);
    }

    /// Plays or pauses, whichever the state calls for.
    pub fn toggle(&self) {
        let _ = self.cmd.send(Cmd::Toggle);
    }

    /// Jumps to `t` seconds from the start.
    pub fn seek(&self, t: f64) {
        let _ = self.cmd.send(Cmd::Seek(t));
    }

    /// Jumps to a fraction (0..1) of the length.
    pub fn seek_fraction(&self, f: f64) {
        let d = self.state().duration;
        self.seek(f.clamp(0.0, 1.0) * d);
    }

    /// 0..1.
    pub fn set_volume(&self, v: f32) {
        let _ = self.cmd.send(Cmd::Volume(v));
    }

    pub fn set_muted(&self, m: bool) {
        let _ = self.cmd.send(Cmd::Mute(m));
    }

    pub fn set_looping(&self, l: bool) {
        let _ = self.cmd.send(Cmd::Looping(l));
    }

    pub fn set_background(&self, rgba: [f32; 4]) {
        let _ = self.cmd.send(Cmd::Background(rgba));
    }

    /// Draws again (after the window was uncovered, say).
    pub fn redraw(&self) {
        let _ = self.cmd.send(Cmd::Redraw);
    }

    /// The latest snapshot of the playback.
    pub fn state(&self) -> WindowState {
        self.shared.state.lock().unwrap().clone()
    }

    /// What happened to the window since the last call.
    pub fn take_events(&self) -> Vec<WindowEvent> {
        self.shared.events.lock().unwrap().drain(..).collect()
    }

    /// Moves and resizes the window the way it was placed: screen coordinates for a popup,
    /// parent client coordinates for a child. No effect on an [`Placement::Existing`] window.
    pub fn set_rect(&self, x: i32, y: i32, width: u32, height: u32) {
        if self.owns_window {
            unsafe {
                let _ = SetWindowPos(HWND(self.hwnd as *mut _), None, x, y, width.max(1) as i32, height.max(1) as i32, SWP_NOACTIVATE | SWP_NOZORDER);
            }
        }
    }

    pub fn set_visible(&self, visible: bool) {
        if self.owns_window {
            unsafe {
                let _ = ShowWindow(HWND(self.hwnd as *mut _), if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
    }

    /// Reads the back buffer as it is drawn for the next present (forces one): what the
    /// window shows, letterbox and all. Waits up to two seconds.
    pub fn capture(&self) -> std::result::Result<Capture, String> {
        let (tx, rx) = channel();
        self.cmd.send(Cmd::Capture(tx)).map_err(|_| "the window is closed".to_string())?;
        rx.recv_timeout(Duration::from_secs(2)).map_err(|_| "no answer from the render thread".to_string())?
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // wake the render thread out of its wait
        let _ = self.cmd.send(Cmd::Redraw);
        if let Some(h) = self.render.take() {
            let _ = h.join();
        }
        if self.owns_window {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            if let Some(w) = self.window.take() {
                let _ = w.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_keeps_the_aspect() {
        // 16:9 in a 4:3 window: bars above and below
        assert_eq!(letterbox(800, 600, 1920, 1080), (0, 75, 800, 450));
        // 16:9 in a wide window: bars at the sides
        assert_eq!(letterbox(1000, 400, 1920, 1080), (144, 0, 711, 400));
        // portrait video
        assert_eq!(letterbox(600, 600, 1080, 1920), (131, 0, 338, 600));
        // exact fit, and nothing to fit
        assert_eq!(letterbox(640, 360, 1920, 1080), (0, 0, 640, 360));
        assert_eq!(letterbox(0, 0, 10, 10), (0, 0, 0, 0));
    }

    #[test]
    fn display_size_follows_the_pixel_aspect_ratio() {
        // square pixels, and a stream that does not say anything
        assert_eq!(shown_size((640, 360), (640, 360), (640, 360)), (640, 360));
        assert_eq!(shown_size((640, 360), (640, 360), (0, 0)), (640, 360));
        // 4:3 pixels: 853 x 360 on show, in a 4:3 window the bars are above and below
        assert_eq!(shown_size((640, 360), (640, 360), (853, 360)), (853, 360));
        assert_eq!(letterbox(800, 600, 853, 360), (0, 131, 800, 338));
        // a frame of another size than the stream's first one keeps the pixel shape
        assert_eq!(shown_size((320, 180), (640, 360), (853, 360)), (427, 180));
    }
}
