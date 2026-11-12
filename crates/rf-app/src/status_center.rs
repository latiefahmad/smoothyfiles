//! Status Center: in-app progress for copy/move/delete with cancel (Files' "StatusCenter").
//! Work runs on its own thread; progress is throttled before it reaches the UI.

use crate::t;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_win::fileops::{self, Tracker, Transfer};

use crate::theme::{self, Palette};

#[derive(Clone, PartialEq)]
pub enum OpPhase {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

pub struct OpState {
    id: u64,
    title: String,
    total: u32,
    done: u32,
    current: Option<String>,
    phase: OpPhase,
    cancel: Arc<AtomicBool>,
    /// Our copy engine waits while set (the shell engine ignores it).
    paused: Arc<AtomicBool>,
    started: Instant,
}

pub struct Ops {
    ops: Vec<OpState>,
    next: u64,
    pub open: bool,
    /// A failure the user has not looked at yet (the button turns red until opened).
    unseen_failure: bool,
    /// The window whose button opened the flyout (it is anchored there).
    owner: Option<AnyWindowHandle>,
    /// The Acrylic popup could not be opened: draw the flyout inside the window instead.
    in_window: bool,
    /// When the popup closed itself (focus left it): a click on the button right after must
    /// not reopen it.
    auto_closed: Option<Instant>,
}

/// Finished operations kept in the list (Files keeps them until "Clear").
const HISTORY: usize = 30;

pub struct OpsGlobal(pub Entity<Ops>);
impl Global for OpsGlobal {}

enum Ev {
    Progress(u32, u32, Option<String>),
    /// The copy thread asks about names that already exist and waits for the reply.
    Ask(Vec<rf_win::fastcopy::Conflict>, crate::conflict_dialog::Reply),
    Done(Result<bool, String>),
}

impl Ops {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { ops: Vec::new(), next: 1, open: false, unseen_failure: false, owner: None, in_window: false, auto_closed: None }
    }

    pub fn toggle(&mut self, owner: AnyWindowHandle, cx: &mut Context<Self>) {
        if !self.open && self.auto_closed.is_some_and(|t| t.elapsed() < Duration::from_millis(350)) {
            // this click is what closed it (the popup lost focus to the button)
            self.auto_closed = None;
            return;
        }
        self.owner = Some(owner);
        self.open = !self.open;
        if self.open {
            self.unseen_failure = false;
        }
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.notify();
        }
    }

    fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.ops.retain(|o| o.phase == OpPhase::Running);
        self.unseen_failure = false;
        cx.notify();
    }

    /// Title, outcome and duration of an operation (for the "finished" notification).
    pub fn summary(&self, id: u64) -> Option<(String, OpPhase, Duration)> {
        self.ops.iter().find(|o| o.id == id).map(|o| (o.title.clone(), o.phase.clone(), o.started.elapsed()))
    }

    pub fn running(&self) -> usize {
        self.ops.iter().filter(|o| o.phase == OpPhase::Running).count()
    }

    /// What the taskbar buttons show: the running operations' progress, yellow when all are
    /// paused, red while a failure is unseen.
    fn taskbar(&self) -> rf_win::taskbar_progress::State {
        use rf_win::taskbar_progress::State;
        let run: Vec<&OpState> = self.ops.iter().filter(|o| o.phase == OpPhase::Running).collect();
        let Some(v) = self.overall() else { return State::None };
        if run.iter().all(|o| o.total == 0) {
            State::Busy
        } else if self.unseen_failure {
            State::Error(v)
        } else if run.iter().all(|o| o.paused.load(Ordering::Relaxed)) {
            State::Paused(v)
        } else {
            State::Normal(v)
        }
    }

    fn overall(&self) -> Option<f32> {
        let run: Vec<&OpState> = self.ops.iter().filter(|o| o.phase == OpPhase::Running).collect();
        if run.is_empty() {
            return None;
        }
        let f: f32 = run.iter().map(|o| if o.total > 0 { o.done as f32 / o.total as f32 } else { 0.0 }).sum();
        Some(f / run.len() as f32)
    }
}

/// Starts a tracked operation. `work` runs on a new thread with COM initialized.
pub fn start(
    cx: &mut App,
    title: String,
    work: impl FnOnce(Arc<Tracker>) -> anyhow::Result<bool> + Send + 'static,
) {
    start_with(cx, title, work, None)
}

/// Called on the UI thread when the operation ends; `true` = it succeeded.
pub type OnDone = Box<dyn FnOnce(bool, &mut App)>;

/// Like [`start`], with a callback when the work is over (e.g. refresh a view after `git pull`).
pub fn start_with(
    cx: &mut App,
    title: String,
    work: impl FnOnce(Arc<Tracker>) -> anyhow::Result<bool> + Send + 'static,
    on_done: Option<OnDone>,
) {
    let Some(entity) = cx.try_global::<OpsGlobal>().map(|g| g.0.clone()) else { return };
    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    // questions about the operation go to the window it was started from
    let origin = cx.active_window();
    let (tx, mut rx) = mpsc::unbounded::<Ev>();
    let id = entity.update(cx, |ops, cx| {
        let id = ops.next;
        ops.next += 1;
        ops.ops.push(OpState {
            id,
            title,
            total: 0,
            done: 0,
            current: None,
            phase: OpPhase::Running,
            cancel: cancel.clone(),
            paused: paused.clone(),
            started: Instant::now(),
        });
        cx.notify();
        id
    });

    let ptx = tx.clone();
    let asking_cancel = cancel.clone();
    let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));
    let mut tracker = Tracker::new(
        Box::new(move |t, d, cur| {
            // ~20 updates/s max; item names always go through.
            let mut l = last.lock().unwrap();
            if cur.is_some() || l.elapsed() >= Duration::from_millis(50) || d >= t {
                *l = Instant::now();
                let _ = ptx.unbounded_send(Ev::Progress(t, d, cur));
            }
        }),
        cancel,
    );
    tracker.paused = paused;
    let atx = tx.clone();
    tracker.ask = Some(Box::new(move |list| {
        let (rtx, rrx) = std::sync::mpsc::channel();
        atx.unbounded_send(Ev::Ask(list.to_vec(), rtx)).ok()?;
        // no reply (the dialog went away) or the operation cancelled meanwhile = cancel
        loop {
            match rrx.recv_timeout(Duration::from_millis(200)) {
                Ok(answer) => return answer,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !asking_cancel.load(Ordering::Relaxed) => {}
                Err(_) => return None,
            }
        }
    }));
    let tracker = Arc::new(tracker);
    std::thread::Builder::new()
        .name("rf-op".into())
        .spawn(move || {
            rf_win::shell::ensure_com();
            let r = work(tracker).map_err(|e| format!("{e:#}"));
            let _ = tx.unbounded_send(Ev::Done(r));
        })
        .ok();

    cx.spawn(async move |cx| {
        let mut ended = false;
        let mut ok = false;
        while let Some(first) = rx.next().await {
            let mut evs = vec![first];
            while let Ok(e) = rx.try_recv() {
                evs.push(e);
            }
            let mut finished = false;
            let mut asks = Vec::new();
            let _ = entity.update(cx, |ops, cx| {
                if let Some(op) = ops.ops.iter_mut().find(|o| o.id == id) {
                    for e in evs {
                        match e {
                            Ev::Ask(list, reply) => {
                                op.current = Some(t!("Hedefte aynı adlı öğeler var").to_string());
                                asks.push((list, reply));
                            }
                            Ev::Progress(t, d, cur) => {
                                if t > 0 {
                                    op.total = t;
                                    op.done = d;
                                }
                                if cur.is_some() {
                                    op.current = cur;
                                }
                            }
                            Ev::Done(r) => {
                                finished = true;
                                ok = matches!(r, Ok(true));
                                op.phase = match r {
                                    Ok(true) => OpPhase::Done,
                                    Ok(false) => OpPhase::Cancelled,
                                    Err(e) => OpPhase::Failed(e),
                                };
                                op.done = op.total;
                            }
                        }
                    }
                }
                if finished {
                    if ops.ops.iter().any(|o| o.id == id && matches!(o.phase, OpPhase::Failed(_))) && !ops.open {
                        ops.unseen_failure = true;
                    }
                    // keep a bounded history of finished operations (oldest finished go first)
                    while ops.ops.len() > HISTORY {
                        let Some(ix) = ops.ops.iter().position(|o| o.phase != OpPhase::Running) else { break };
                        ops.ops.remove(ix);
                    }
                }
                cx.notify();
            });
            for (list, reply) in asks {
                let _ = cx.update(|cx| crate::conflict_dialog::show(list, reply, origin, cx));
            }
            if finished {
                ended = true;
                break;
            }
        }
        if !ended {
            // worker died without reporting (panic / spawn failure): don't spin forever
            let _ = entity.update(cx, |ops, cx| {
                if let Some(op) = ops.ops.iter_mut().find(|o| o.id == id) {
                    op.phase = OpPhase::Failed(t!("İşlem beklenmedik şekilde sonlandı").into());
                }
                ops.unseen_failure |= !ops.open;
                cx.notify();
            });
        }
        let _ = cx.update(|cx| crate::background::op_finished(&entity, id, cx));
        if let Some(done) = on_done {
            let _ = cx.update(|cx| done(ok, cx));
        }
    })
    .detach();
}

fn short(p: &Path) -> String {
    rf_core::history::display_name(p)
}

/// Copy/move with progress in the Status Center.
pub fn transfer(cx: &mut App, kind: Transfer, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf) {
    transfer_ex(cx, kind, sources, dest, true)
}

/// Like [`transfer`], then `after` on the UI thread with the items that arrived at the top of
/// `dest` (new copies, merged folders), e.g. to select them.
pub fn transfer_then(cx: &mut App, kind: Transfer, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf, after: impl FnOnce(Vec<std::path::PathBuf>, &mut App) + 'static) {
    transfer_inner(cx, kind, sources, dest, true, Some(Box::new(after)));
}

/// `record = false` for undo/redo replays (they must not land on the undo stack again).
pub fn transfer_ex(cx: &mut App, kind: Transfer, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf, record: bool) {
    transfer_inner(cx, kind, sources, dest, record, None);
}

#[allow(clippy::type_complexity)]
fn transfer_inner(
    cx: &mut App,
    kind: Transfer,
    sources: Vec<std::path::PathBuf>,
    dest: std::path::PathBuf,
    record: bool,
    after: Option<Box<dyn FnOnce(Vec<std::path::PathBuf>, &mut App)>>,
) {
    // into a folder inside an archive: add to the archive (copy semantics; RAR/ISO refuse)
    if let Some((archive, inner)) = crate::archive_view::split(&dest) {
        crate::archive_view::add_into(cx, archive, inner, sources);
        return;
    }
    let verb = match kind {
        Transfer::Copy => t!("Kopyalanıyor"),
        Transfer::Move => t!("Taşınıyor"),
    };
    let title = format!("{verb}: {} → {}", crate::i18n::items(sources.len()), short(&dest));
    let s = cx.global::<crate::settings::Settings>();
    let (fast, verify, limit) = (s.fast_file_ops, s.copy_verify.engine(), u64::from(s.copy_speed_limit) << 20);
    let arrived = Arc::new(std::sync::Mutex::new(Vec::<std::path::PathBuf>::new()));
    let landed = arrived.clone();
    let top = dest.clone();
    let on_done: Option<OnDone> = after.map(|f| -> OnDone { Box::new(move |_, cx: &mut App| f(std::mem::take(&mut *arrived.lock().unwrap()), cx)) });
    start_with(cx, title, move |t| {
        t.set_verify(verify);
        t.limit.store(limit, Ordering::Relaxed);
        // our engine (conflicts asked in one list); Windows' engine when it is turned off
        let r = if fast {
            rf_win::fastcopy::transfer(kind, &sources, &dest, t.clone())
        } else {
            fileops::transfer_tracked(kind, &sources, &dest, t.clone())
        };
        let pairs = t.results.lock().unwrap().clone();
        // what now sits directly in the destination: the new item, or the folder a merge went into
        let mut tops: Vec<std::path::PathBuf> = Vec::new();
        for (_, dst) in &pairs {
            if let Some(first) = dst.strip_prefix(&top).ok().and_then(|rel| rel.components().next()) {
                let p = top.join(first);
                if !tops.contains(&p) {
                    tops.push(p);
                }
            }
        }
        *landed.lock().unwrap() = tops;
        if record && !pairs.is_empty() {
            crate::undo::push(crate::undo::UndoOp::Transferred { kind, pairs });
        }
        r
    }, on_done);
}

/// A copy or move cut off by a crash or a shutdown: asked about once the main window is up
/// (Resume / Discard / Later), one job at a time.
pub fn offer_resume(cx: &mut App) {
    let found = cx.background_spawn(async {
        // let the window settle first; the scan itself is a directory listing
        std::thread::sleep(std::time::Duration::from_millis(700));
        rf_win::fastcopy::interrupted()
    });
    cx.spawn(async move |cx| {
        let list = found.await;
        let Some(job) = list.into_iter().next() else { return };
        cx.update(|cx| {
            let Some(w) = cx.active_window().or_else(|| cx.windows().first().copied()) else { return };
            let _ = w.update(cx, |_, window, cx| ask_resume(job, window, cx));
        });
    })
    .detach();
}

fn ask_resume(job: rf_win::fastcopy::Interrupted, window: &mut gpui_kit::Window, cx: &mut App) {
    use crate::prompt::{Button, Kind};
    let title = match job.kind {
        Transfer::Copy => t!("Yarıda kalan bir kopyalama var"),
        Transfer::Move => t!("Yarıda kalan bir taşıma var"),
    };
    let what = format!("{} → {}", crate::i18n::items(job.sources.len()), short(&job.dest));
    let msg = format!(
        "{what}\n{} / {} {}\n\n{}",
        job.done.min(job.files),
        job.files,
        t!("dosya tamamlandı"),
        t!("Kaldığı yerden devam edilsin mi? Tamamlanan dosyalar kaynaklarıyla yeniden karşılaştırılır, yarım kalanlar baştan kopyalanır.")
    );
    let (j1, j2) = (job.clone(), job.clone());
    crate::prompt::show(
        title,
        Some(msg),
        None,
        vec![
            Button::new(t!("Devam et"), Kind::Primary, move |_, _, cx| {
                resume_job(cx, j1.clone());
                offer_resume(cx);
                Ok(())
            }),
            Button::new(t!("Vazgeç"), Kind::Normal, move |_, _, cx| {
                // what was copied stays where it is
                rf_win::fastcopy::discard(&j2.journal);
                offer_resume(cx);
                Ok(())
            }),
            Button::new(t!("Sonra"), Kind::Normal, |_, _, _| Ok(())),
        ],
        window,
        cx,
    );
}

/// Carries a cut-off job on in the Status Center, with its original plan.
pub fn resume_job(cx: &mut App, job: rf_win::fastcopy::Interrupted) {
    let verb = match job.kind {
        Transfer::Copy => t!("Kopyalanıyor"),
        Transfer::Move => t!("Taşınıyor"),
    };
    let title = format!("{verb}: {} → {}", crate::i18n::items(job.sources.len()), short(&job.dest));
    let limit = u64::from(cx.global::<crate::settings::Settings>().copy_speed_limit) << 20;
    start(cx, title, move |t| {
        t.limit.store(limit, Ordering::Relaxed);
        rf_win::fastcopy::resume(&job.journal, t)
    });
}

pub fn archive_progress_pub(t: &Tracker) -> impl FnMut(rf_archive::Progress) + '_ {
    archive_progress(t)
}

/// Archive progress (bytes) → tracker units (permille, fits u32).
fn archive_progress(t: &Tracker) -> impl FnMut(rf_archive::Progress) + '_ {
    move |p: rf_archive::Progress| {
        let total = p.total_bytes.max(1);
        let done = (p.done_bytes.min(total) * 1000 / total) as u32;
        (t.on_progress)(1000, done, Some(p.current))
    }
}

/// `dest = None`: "extract to <name>\\" (unique folder chosen on the worker thread).
/// A protected archive asks for its password first (probed off the UI thread).
pub fn extract(cx: &mut App, archive: std::path::PathBuf, dest: Option<std::path::PathBuf>) {
    let probe = cx.background_spawn({
        let a = archive.clone();
        async move { crate::archive_view::needs_password_now(&a) }
    });
    cx.spawn(async move |cx| {
        let need = probe.await;
        cx.update(|cx| {
            if need {
                if let Some(w) = cx.active_window() {
                    let (a, d) = (archive.clone(), dest.clone());
                    let asked = w
                        .update(cx, |_, window, cx| {
                            crate::archive_view::ask_password(archive.clone(), false, window, cx, move |cx| extract_now(cx, a.clone(), d.clone()))
                        })
                        .is_ok();
                    if asked {
                        return;
                    }
                }
            }
            extract_now(cx, archive, dest);
        });
    })
    .detach();
}

fn extract_now(cx: &mut App, archive: std::path::PathBuf, dest: Option<std::path::PathBuf>) {
    let title = match &dest {
        Some(d) => format!("{} {} → {}", t!("Çıkarılıyor:"), short(&archive), short(d)),
        None => format!("{} {}", t!("Çıkarılıyor:"), short(&archive)),
    };
    start(cx, title, move |t| {
        let dest = dest.unwrap_or_else(|| rf_archive::smart_extract_dir(&archive));
        let cancel = t.cancel.clone();
        let mut prog = archive_progress(&t);
        let opts = crate::archive_view::options(&archive);
        match rf_archive::extract_with(&archive, &dest, &opts, &mut prog, &|| cancel.load(Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => {
                if crate::archive_view::password_problem(&e) == Some(true) {
                    // forget it: the next attempt asks again
                    crate::archive_view::set_password(&archive, None);
                }
                Err(e)
            }
        }
    });
}

/// Like `compress_level`, encrypted (ZIP: AES-256, 7z: AES-256 incl. names).
pub fn compress_protected(cx: &mut App, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf, format: rf_archive::Format, level: u32, password: String) {
    let title = format!("{} {} → {}", t!("Sıkıştırılıyor:"), crate::i18n::items(sources.len()), short(&dest));
    start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let mut prog = archive_progress(&t);
        let opts = rf_archive::CompressOptions { password: Some(password) };
        match rf_archive::compress_with(&sources, &dest, format, level, &opts, &mut prog, &|| cancel.load(Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

/// RAR through WinRAR's Rar.exe (`level` 0…9).
pub fn compress_rar(cx: &mut App, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf, level: u32, password: Option<String>) {
    let title = format!("{} {} → {}", t!("Sıkıştırılıyor:"), crate::i18n::items(sources.len()), short(&dest));
    start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let mut prog = archive_progress(&t);
        match rf_archive::compress_rar(&sources, &dest, level, password.as_deref(), &mut prog, &|| cancel.load(Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

pub fn compress(cx: &mut App, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf, format: rf_archive::Format) {
    compress_level(cx, sources, dest, format, 5)
}

/// `level`: 0 store … 9 smallest.
pub fn compress_level(cx: &mut App, sources: Vec<std::path::PathBuf>, dest: std::path::PathBuf, format: rf_archive::Format, level: u32) {
    let title = format!("{} {} → {}", t!("Sıkıştırılıyor:"), crate::i18n::items(sources.len()), short(&dest));
    start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let mut prog = archive_progress(&t);
        match rf_archive::compress(&sources, &dest, format, level, &mut prog, &|| cancel.load(Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

pub fn delete(cx: &mut App, paths: Vec<std::path::PathBuf>, permanent: bool) {
    delete_ex(cx, paths, permanent, true)
}

pub fn delete_ex(cx: &mut App, paths: Vec<std::path::PathBuf>, permanent: bool, record: bool) {
    let title = if permanent {
        format!("{} {}", t!("Kalıcı olarak siliniyor:"), crate::i18n::items(paths.len()))
    } else {
        format!("{} {}", t!("Geri dönüşüm kutusuna taşınıyor:"), crate::i18n::items(paths.len()))
    };
    let fast = cx.global::<crate::settings::Settings>().fast_file_ops;
    start(cx, title, move |t| {
        // permanent: our parallel delete (the shell takes what it can't); recycle bin: the shell
        let r = if permanent && fast {
            rf_win::fastdelete::delete_permanent(&paths, t.clone(), rf_win::fastdelete::threads_for(&paths))
        } else {
            fileops::delete_tracked(&paths, permanent, t.clone())
        };
        let pairs = t.results.lock().unwrap().clone();
        if record && !permanent && !pairs.is_empty() {
            crate::undo::push(crate::undo::UndoOp::Recycled { pairs });
        }
        r
    });
}

/// Title-bar button (top right, like Files): idle icon, a progress bar under it while work
/// runs, red when a failure has not been looked at. Click toggles the flyout.
/// Puts the operations' progress on every main window's taskbar button (when it changed by a
/// tenth of a percent or more).
pub fn sync_taskbar(ops: &Entity<Ops>, cx: &mut App) {
    use rf_win::taskbar_progress::State;
    thread_local! {
        static LAST: std::cell::Cell<(State, usize)> = const { std::cell::Cell::new((State::None, 0)) };
    }
    let state = match ops.read(cx).taskbar() {
        State::Normal(v) => State::Normal((v * 1000.0).round() / 1000.0),
        State::Paused(v) => State::Paused((v * 1000.0).round() / 1000.0),
        State::Error(v) => State::Error((v * 1000.0).round() / 1000.0),
        s => s,
    };
    let windows = crate::workspace::main_windows(cx);
    // (a window opened meanwhile gets the state too)
    if LAST.with(|l| l.get()) == (state, windows.len()) {
        return;
    }
    LAST.with(|l| l.set((state, windows.len())));
    for w in windows {
        let _ = w.update(cx, |_, window, _| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(RawWindowHandle::Win32(h)) = HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                rf_win::taskbar_progress::set(h.hwnd.get(), state);
            }
        });
    }
}

pub fn render_button(ops: &Entity<Ops>, p: &Palette, cx: &App) -> AnyElement {
    let o = ops.read(cx);
    let running = o.running();
    let overall = o.overall();
    let (open, unseen) = (o.open, o.unseen_failure);
    let ops = ops.clone();
    let color = if unseen { p.danger } else if running > 0 { p.accent } else { p.text };
    div()
        .id("status-center")
        // no tooltip while the flyout is open: it would sit on top of the flyout
        .when(!open, |d| d.tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Durum Merkezi")).build(w, cx)))
        .relative()
        .size(px(32.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .text_color(color)
        .when(open, |d| d.bg(p.pressed))
        .when(!open, |d| d.hover(|s| s.bg(p.hover)))
        .active(|s| s.bg(p.pressed))
        .child(Icon::new(if unseen { L::CircleAlert } else { L::ListChecks }).size(px(16.)))
        .when_some(overall, |d, f| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(4.))
                    .left(px(7.))
                    .w(px(18.))
                    .h(px(2.))
                    .rounded_full()
                    .bg(p.pressed)
                    .child(div().h_full().rounded_full().bg(p.accent).w(relative(f.clamp(0.05, 1.0)))),
            )
        })
        .when(running == 0 && matches!(crate::installer::state(), crate::installer::State::Ready(_)), |d| {
            let dot = div().absolute().top(px(5.)).right(px(5.)).size(px(7.)).rounded_full().bg(p.accent);
            // a few seconds of pulse when it becomes ready (an endless one would keep drawing frames)
            let fresh = crate::installer::ready_since().is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(8));
            if fresh {
                let ring = p.accent;
                d.child(dot).child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(theme::RADIUS_SM)
                        .border_2()
                        .border_color(ring)
                        .with_animation(
                            "sc-pulse",
                            Animation::new(std::time::Duration::from_millis(1300)).repeat().with_easing(pulsating_between(0.0, 0.9)),
                            |el, t| el.opacity(t),
                        ),
                )
            } else {
                d.child(dot)
            }
        })
        .when(running > 1, |d| {
            d.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(2.))
                    .min_w(px(14.))
                    .h(px(14.))
                    .px(px(3.))
                    .rounded_full()
                    .bg(p.accent)
                    .text_color(p.accent_text)
                    .text_size(px(9.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(running.to_string()),
            )
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            let owner = window.window_handle();
            ops.update(cx, |o, cx| o.toggle(owner, cx))
        })
        .into_any_element()
}

/// The Status Center flyout under the title-bar button, with a click-away layer behind it.
pub fn render_flyout(ops: &Entity<Ops>, p: &Palette, top: Pixels, cx: &App) -> Option<AnyElement> {
    let o = ops.read(cx);
    if !o.open || !o.in_window {
        return None;
    }
    let any_finished = o.ops.iter().any(|x| x.phase != OpPhase::Running);
    let mut list = v_flex().gap_1();
    // newest first
    for op in o.ops.iter().rev() {
        list = list.child(render_op(ops, op, p));
    }
    let (close_e, clear_e) = (ops.clone(), ops.clone());
    let update = update_row(p);
    let body: AnyElement = if o.ops.is_empty() && update.is_some() {
        div().into_any_element()
    } else if o.ops.is_empty() {
        div()
            .py_6()
            .flex()
            .justify_center()
            .text_sm()
            .text_color(p.text_secondary)
            .child(t!("Devam eden dosya işlemleri yok"))
            .into_any_element()
    } else {
        div().id("sc-list").max_h(px(460.)).overflow_y_scroll().child(list).into_any_element()
    };
    Some(
        div()
            .absolute()
            .inset_0()
            // click-away layer
            .child(
                div()
                    .id("sc-away")
                    .absolute()
                    .inset_0()
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| close_e.update(cx, |o, cx| o.close(cx)))
                    .on_mouse_down(MouseButton::Right, {
                        let e = ops.clone();
                        move |_, _, cx| e.update(cx, |o, cx| o.close(cx))
                    }),
            )
            .child(
                v_flex()
                    .id("sc-flyout")
                    .absolute()
                    .top(top)
                    .right(px(8.))
                    .w(px(400.))
                    .p_1()
                    .rounded(theme::RADIUS)
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .px_3()
                            .pt_2()
                            .pb_2()
                            .items_center()
                            .child(div().flex_1().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("Durum Merkezi")))
                            .child(
                                div()
                                    .id("sc-clear")
                                    .px_2()
                                    .py_1()
                                    .rounded(theme::RADIUS_SM)
                                    .text_xs()
                                    .border_1()
                                    .border_color(p.border)
                                    .when(!any_finished, |d| d.text_color(p.text_tertiary))
                                    .when(any_finished, |d| {
                                        d.hover(|s| s.bg(p.hover))
                                            .active(|s| s.bg(p.pressed))
                                            .on_click(move |_, _, cx| clear_e.update(cx, |o, cx| o.clear_finished(cx)))
                                    })
                                    .child(t!("Tamamlananları temizle")),
                            ),
                    )
                    .children(update)
                    .child(body),
            )
            .into_any_element(),
    )
}

fn render_op(ops: &Entity<Ops>, op: &OpState, p: &Palette) -> AnyElement {
    let frac = if op.total > 0 { op.done as f32 / op.total as f32 } else { 0.0 };
    let cancel = op.cancel.clone();
    let pause = op.paused.clone();
    let is_paused = op.paused.load(Ordering::Relaxed);
    let running = op.phase == OpPhase::Running;
    let (ops_e, op_id) = (ops.clone(), op.id);
    let (status, color, icon) = match &op.phase {
        OpPhase::Running if is_paused => (t!("Duraklatıldı").to_string(), p.text_secondary, L::Pause),
        OpPhase::Running => {
            let elapsed = op.started.elapsed().as_secs_f32();
            // ETA from the average rate so far; only once there is enough signal
            let eta = (frac > 0.03 && frac < 1.0 && elapsed > 1.5).then(|| elapsed * (1.0 - frac) / frac);
            let text = if op.total == 0 {
                format!("{} {}", elapsed as u64, t!("sn"))
            } else {
                let pct = crate::i18n::pct(frac * 100.0);
                match eta {
                    Some(r) => format!("{pct} · {} {}", fmt_duration(r), t!("kaldı")),
                    None => format!("{pct} · {} {}", elapsed as u64, t!("sn")),
                }
            };
            (text, p.text_secondary, L::LoaderCircle)
        }
        OpPhase::Done => (t!("Tamamlandı").to_string(), p.accent, L::CircleCheck),
        OpPhase::Cancelled => (t!("İptal edildi").to_string(), p.text_secondary, L::CircleSlash),
        OpPhase::Failed(e) => (format!("{} {}", t!("Hata:"), crate::i18n::td(e)), p.danger, L::CircleAlert),
    };
    let icon_color = match &op.phase {
        OpPhase::Running => p.accent,
        OpPhase::Done => gpui_kit::rgb(0x30D158).into(),
        _ => color,
    };
    h_flex()
        .id(("op", op.id as usize))
        .group("op")
        .gap_3()
        .px_3()
        .py_2()
        .items_start()
        .rounded(theme::RADIUS_SM)
        .hover(|s| s.bg(p.hover))
        .child(div().pt(px(2.)).child(Icon::new(icon).size(px(16.)).text_color(icon_color)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_sm().overflow_hidden().text_ellipsis().whitespace_nowrap().child(op.title.clone()))
                .when(running, |d| {
                    d.child(
                        div()
                            .h(px(3.))
                            .w_full()
                            .rounded_full()
                            .bg(p.pressed)
                            // unknown total (git, network): a short accent segment
                            .child(div().h_full().rounded_full().bg(p.accent).w(relative(if op.total == 0 { 0.15 } else { frac.max(0.02) }))),
                    )
                })
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(p.text_secondary)
                        .child(div().flex_shrink_0().text_color(color).child(status))
                        .children(op.current.clone().filter(|_| running).map(|c| {
                            div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(c)
                        })),
                ),
        )
        .when(running, |d| {
            d.child(
                div()
                    .id(("op-pause", op.id as usize))
                    .size(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .text_color(p.text_secondary)
                    .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(if is_paused { t!("Devam et") } else { t!("Duraklat") }).build(w, cx))
                    .hover(|s| s.bg(p.pressed))
                    .child(Icon::new(if is_paused { L::Play } else { L::Pause }).size(px(12.)))
                    .on_click({
                        let ops_e = ops.clone();
                        move |_, _, cx| {
                            pause.store(!is_paused, Ordering::Relaxed);
                            ops_e.update(cx, |_, cx| cx.notify());
                        }
                    }),
            )
        })
        .child(
            div()
                .id(("op-x", op.id as usize))
                .size(px(24.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS_SM)
                .text_color(p.text_secondary)
                .tooltip(move |w, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(if running { t!("İptal") } else { t!("Kapat") }).build(w, cx)
                })
                .hover(|s| s.bg(p.pressed))
                .child(Icon::new(L::X).size(px(12.)))
                .on_click(move |_, _, cx| {
                    if running {
                        cancel.store(true, Ordering::Relaxed);
                    } else {
                        ops_e.update(cx, |o, cx| {
                            o.ops.retain(|x| x.id != op_id);
                            cx.notify();
                        })
                    }
                }),
        )
        .into_any_element()
}

/// "~45 sn", "~3 dk", "~1 sa 5 dk" (rounded; it is an estimate).
fn fmt_duration(secs: f32) -> String {
    fmt_duration_with(secs, (crate::t!("sn"), crate::t!("dk"), crate::t!("sa")))
}

/// Unit labels are parameters so the formatting is testable without the global language.
fn fmt_duration_with(secs: f32, (sec, min, hour): (&str, &str, &str)) -> String {
    let s = secs.max(1.0).round() as u64;
    if s < 60 {
        return format!("~{s} {sec}");
    }
    // round to whole minutes first, then roll over (3590 s is "~1 sa", never "~60 dk")
    let total_min = (s + 30) / 60;
    if total_min < 60 {
        format!("~{total_min} {min}")
    } else {
        format!("~{} {hour} {} {min}", total_min / 60, total_min % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_duration_with;

    #[test]
    fn durations() {
        let u = ("sn", "dk", "sa");
        assert_eq!(fmt_duration_with(0.2, u), "~1 sn");
        assert_eq!(fmt_duration_with(45.0, u), "~45 sn");
        assert_eq!(fmt_duration_with(150.0, u), "~3 dk");
        assert_eq!(fmt_duration_with(3580.0, u), "~1 sa 0 dk");
        assert_eq!(fmt_duration_with(3900.0, u), "~1 sa 5 dk");
    }
}

// ---------- Acrylic popup (Windows 11 flyout material) ----------

const FLYOUT_W: f32 = 400.;

struct ScPopup(Option<AnyWindowHandle>);
impl Global for ScPopup {}

/// Keeps the popup window in step with `Ops::open` (call from an observer of the ops entity).
pub fn sync_popup(ops: &Entity<Ops>, cx: &mut App) {
    let (open, owner, in_window) = {
        let o = ops.read(cx);
        (o.open, o.owner, o.in_window)
    };
    let cur = cx.try_global::<ScPopup>().and_then(|g| g.0);
    match (open, cur) {
        (true, None) if !in_window => {
            let Some(owner) = owner else { return };
            // the app window keeps its active look (Mica, caption) while the flyout has focus
            gpui_windows::KEEP_ACTIVE_LOOK.store(true, std::sync::atomic::Ordering::Relaxed);
            let opened = if std::env::var_os("RF_INWINDOW_MENU").is_some() { None } else { open_popup(ops, owner, cx) };
            match opened {
                Some(h) => cx.set_global(ScPopup(Some(h))),
                None => ops.update(cx, |o, cx| {
                    gpui_windows::KEEP_ACTIVE_LOOK.store(false, std::sync::atomic::Ordering::Relaxed);
                    o.in_window = true;
                    cx.notify();
                }),
            }
        }
        (false, Some(h)) => {
            gpui_windows::KEEP_ACTIVE_LOOK.store(false, std::sync::atomic::Ordering::Relaxed);
            cx.set_global(ScPopup(None));
            let _ = h.update(cx, |_, w, _| w.remove_window());
        }
        (false, None) if in_window => ops.update(cx, |o, _| o.in_window = false),
        _ => {}
    }
}

fn open_popup(ops: &Entity<Ops>, owner: AnyWindowHandle, cx: &mut App) -> Option<AnyWindowHandle> {
    // anchored under the title-bar button: right edge 8 px in from the window's
    let (origin, width, dark) = owner
        .update(cx, |_, w, _| (w.bounds().origin, w.viewport_size().width, Palette::for_window(w).dark))
        .ok()?;
    let top = theme::TITLEBAR_H + px(44.);
    let x = origin.x + width - px(8. + FLYOUT_W);
    let bounds = Bounds::new(point(x, origin.y + top), size(px(FLYOUT_W), px(120.)));
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };
    let ops = ops.clone();
    let r = cx.open_window(opts, move |w, cx| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = w.window_handle() {
            if let RawWindowHandle::Win32(h) = h.as_raw() {
                rf_win::window::set_dark_mode(h.hwnd.get(), dark);
            }
        }
        cx.new(|cx| ScView::new(ops, w, cx))
    });
    match r {
        Ok(h) => Some(h.into()),
        Err(e) => {
            log::warn!("status center popup: {e:#}");
            None
        }
    }
}

/// The Status Center in its own Acrylic window; sizes itself to its content.
struct ScView {
    ops: Entity<Ops>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl ScView {
    fn new(ops: Entity<Ops>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.observe(&ops, |_, _, cx| cx.notify()),
            // clicking anywhere else closes it, like a Windows 11 flyout
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if window.is_window_active() {
                    return;
                }
                let ops = this.ops.clone();
                cx.defer(move |cx| {
                    ops.update(cx, |o, cx| {
                        if o.open {
                            o.open = false;
                            o.auto_closed = Some(Instant::now());
                            cx.notify();
                        }
                    })
                });
            }),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { ops, focus, _subs: subs }
    }
}

impl Render for ScView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let ops = self.ops.clone();
        let panel = panel(&ops, &p, cx);
        let vh = window.viewport_size().height;
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .text_color(p.text)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.ops.update(cx, |o, cx| o.close(cx));
                }
            }))
            .child(
                div()
                    .relative()
                    .w_full()
                    .child(panel)
                    // measure the content; the window follows its height
                    .child(
                        canvas(
                            move |b, window, _| {
                                let h = b.size.height.ceil();
                                if (f32::from(h) - f32::from(vh)).abs() > 1. {
                                    window.on_next_frame(move |w, _| w.resize(size(px(FLYOUT_W), h)));
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
    }
}

/// Header + list for the popup.
fn panel(ops: &Entity<Ops>, p: &Palette, cx: &App) -> Div {
    let o = ops.read(cx);
    let any_finished = o.ops.iter().any(|x| x.phase != OpPhase::Running);
    let mut list = v_flex().gap_1();
    for op in o.ops.iter().rev() {
        list = list.child(render_op(ops, op, p));
    }
    let clear_e = ops.clone();
    let update = update_row(p);
    let body: AnyElement = if o.ops.is_empty() && update.is_some() {
        div().into_any_element()
    } else if o.ops.is_empty() {
        div().py_6().flex().justify_center().text_sm().text_color(p.text_secondary).child(t!("Devam eden dosya işlemleri yok")).into_any_element()
    } else {
        div().id("sc-list").max_h(px(460.)).overflow_y_scroll().child(list).into_any_element()
    };
    v_flex()
        .w_full()
        .p_1()
        .child(
            h_flex()
                .px_3()
                .pt_2()
                .pb_2()
                .items_center()
                .child(div().flex_1().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("Durum Merkezi")))
                .child(
                    div()
                        .id("sc-clear")
                        .px_2()
                        .py_1()
                        .rounded(theme::RADIUS_SM)
                        .text_xs()
                        .border_1()
                        .border_color(p.border)
                        .when(!any_finished, |d| d.text_color(p.text_tertiary))
                        .when(any_finished, |d| {
                            d.hover(|s| s.bg(p.hover)).active(|s| s.bg(p.pressed)).on_click(move |_, _, cx| clear_e.update(cx, |o, cx| o.clear_finished(cx)))
                        })
                        .child(t!("Tamamlananları temizle")),
                ),
        )
        .children(update)
        .child(body)
}

/// The app's own update, pinned at the top of the Status Center while it downloads and once it
/// is ready (`None` otherwise).
fn update_row(p: &Palette) -> Option<AnyElement> {
    use crate::installer::State;
    let (title, detail, pct, ready) = match crate::installer::state() {
        State::Downloading(v, pct) => (t!("smoothyfiles {v} indiriliyor").replace("{v}", &v), format!("%{pct}"), Some(pct), false),
        State::Ready(v) => (t!("smoothyfiles {v} hazır").replace("{v}", &v), t!("Yeniden başlatınca kurulur").to_string(), None, true),
        _ => return None,
    };
    Some(
        v_flex()
            .mx_1()
            .mb_1()
            .px_3()
            .py_2p5()
            .gap_2()
            .rounded(theme::RADIUS_SM)
            .bg(p.selection)
            .border_1()
            .border_color(p.accent)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(Icon::new(if ready { L::RefreshCw } else { L::Download }).size(px(16.)).text_color(p.accent))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
                            .child(div().text_xs().text_color(p.text_secondary).child(detail)),
                    )
                    .when(ready, |d| {
                        d.child(
                            div()
                                .id("sc-update-restart")
                                .px_3()
                                .py_1()
                                .rounded(theme::RADIUS_SM)
                                .bg(p.accent)
                                .text_color(p.accent_text)
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.9))
                                .child(t!("Yeniden başlat"))
                                .on_click(|_, _, _| {
                                    std::thread::spawn(crate::installer::restart_to_update);
                                }),
                        )
                    }),
            )
            .children(pct.map(|pct| {
                div().h(px(3.)).w_full().rounded_full().bg(p.pressed).child(div().h_full().rounded_full().bg(p.accent).w(relative((pct as f32 / 100.).clamp(0.02, 1.0))))
            }))
            .into_any_element(),
    )
}

/// The version whose "update ready" toast was closed (it doesn't come back for that version).
static TOAST_CLOSED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// A small card at the bottom of the window once an update is downloaded: the version, Restart
/// and Later. Slides in once; "Later" (or restarting) hides it for that version.
pub fn update_toast(p: &Palette, picker: bool) -> Option<AnyElement> {
    let crate::installer::State::Ready(v) = crate::installer::state() else { return None };
    // file dialogs of other apps don't get it
    if picker || TOAST_CLOSED.lock().unwrap().as_deref() == Some(v.as_str()) {
        return None;
    }
    let ver = v.clone();
    Some(
        div()
            .absolute()
            .bottom(px(44.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                h_flex()
                    .id("update-toast")
                    .gap_3()
                    .pl_4()
                    .pr_2()
                    .py_2()
                    .items_center()
                    .rounded(theme::RADIUS)
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.accent)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Icon::new(L::RefreshCw).size(px(16.)).text_color(p.accent))
                    .child(
                        v_flex()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("smoothyfiles {v} hazır").replace("{v}", &v)))
                            .child(div().text_xs().text_color(p.text_secondary).child(t!("Yeniden başlatınca kurulur"))),
                    )
                    .child(
                        div()
                            .id("update-toast-restart")
                            .ml_2()
                            .px_3()
                            .py_1p5()
                            .rounded(theme::RADIUS_SM)
                            .bg(p.accent)
                            .text_color(p.accent_text)
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .child(t!("Yeniden başlat"))
                            .on_click(|_, _, _| {
                                std::thread::spawn(crate::installer::restart_to_update);
                            }),
                    )
                    .child(
                        div()
                            .id("update-toast-later")
                            .px_3()
                            .py_1p5()
                            .rounded(theme::RADIUS_SM)
                            .text_sm()
                            .text_color(p.text_secondary)
                            .cursor_pointer()
                            .hover(|s| s.bg(p.hover))
                            .child(t!("Sonra"))
                            .on_click(move |_, window, _| {
                                *TOAST_CLOSED.lock().unwrap() = Some(ver.clone());
                                window.refresh();
                            }),
                    )
                    .with_animation(("update-toast-in", v.len()), Animation::new(std::time::Duration::from_millis(260)).with_easing(ease_out_quint()), |el, t| {
                        el.opacity(t).mb(px(-12. * (1. - t)))
                    }),
            )
            .into_any_element(),
    )
}
