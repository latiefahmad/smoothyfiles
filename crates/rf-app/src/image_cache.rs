//! Shared cache of shell icons and thumbnails. Misses are queued to rf-win's low-priority
//! worker pool; rows render a symbol fallback until the real image arrives.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui_kit::*;
use rf_core::Entry;
use rf_win::thumbs::{Bgra, Kind, Request, Response, ThumbPool, has_unique_icon};

enum Slot {
    Pending,
    Ready(Arc<RenderImage>),
    Failed,
}

pub struct ImageCache {
    pool: ThumbPool,
    map: HashMap<u64, Slot>,
    /// Thumbnails (and per-file icons): key → (last use, pixel bytes). Evicted least recently
    /// used first once over `THUMB_BUDGET`; generic icons are few and stay.
    thumb_use: HashMap<u64, (u64, usize)>,
    /// Running sum of `cost()` over `thumb_use`.
    thumb_cost: usize,
    tick: u64,
    /// Evicted images whose GPU atlas tiles must be released (see `release_evicted`).
    evicted: Vec<Arc<RenderImage>>,
    _task: Task<()>,
}

/// Pixel budget for thumbnails. By bytes, not count: a 4K screen of small tiles shows hundreds
/// at once (a count cap would evict on-screen images every frame), large tiles cost ~300 KB each.
const THUMB_BUDGET: usize = 64 * 1024 * 1024;
/// Minimum charge per tracked entry, so pending/failed slots are bounded too.
const NOMINAL: usize = 16 * 1024;

fn cost(bytes: usize) -> usize {
    bytes.max(NOMINAL)
}
pub const ICON_PX: u32 = 32;

fn key_of(parts: &[&dyn HashPart]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash_part(&mut h);
    }
    h.finish()
}

trait HashPart {
    fn hash_part(&self, h: &mut std::collections::hash_map::DefaultHasher);
}
impl<T: Hash> HashPart for T {
    fn hash_part(&self, h: &mut std::collections::hash_map::DefaultHasher) {
        self.hash(h)
    }
}

pub(crate) fn to_render_image(b: &Bgra) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(b.width, b.height, b.data.clone())?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
}

impl ImageCache {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::unbounded::<Response>();
        let workers = std::thread::available_parallelism().map(|n| (n.get() / 4).clamp(2, 4)).unwrap_or(2);
        let pool = ThumbPool::new(workers, move |r| {
            let _ = tx.unbounded_send(r);
        });
        let task = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(r) = rx.try_recv() {
                    batch.push(r);
                }
                let ok = this
                    .update(cx, |c, cx| {
                        for r in batch {
                            if r.dropped {
                                c.map.remove(&r.key);
                                continue;
                            }
                            // evicted while it was being made: nobody wants it any more
                            if !matches!(c.map.get(&r.key), Some(Slot::Pending)) {
                                continue;
                            }
                            let slot = match r.image.as_deref().and_then(to_render_image) {
                                Some(img) => {
                                    if let Some(u) = c.thumb_use.get_mut(&r.key) {
                                        let bytes = r.image.as_ref().map_or(0, |b| b.data.len());
                                        c.thumb_cost = c.thumb_cost - cost(u.1) + cost(bytes);
                                        u.1 = bytes;
                                    }
                                    Slot::Ready(img)
                                }
                                None => Slot::Failed,
                            };
                            c.map.insert(r.key, slot);
                        }
                        c.evict_over_budget();
                        c.release_evicted(cx);
                        cx.notify();
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        });
        Self {
            pool,
            map: HashMap::new(),
            thumb_use: HashMap::new(),
            thumb_cost: 0,
            tick: 0,
            evicted: Vec::new(),
            _task: task,
        }
    }

    fn lookup(&mut self, key: u64, make: impl FnOnce() -> Request, is_thumb: bool) -> Option<Arc<RenderImage>> {
        if is_thumb {
            // every use refreshes recency, so on-screen images are the last to go
            self.tick += 1;
            let tick = self.tick;
            match self.thumb_use.get_mut(&key) {
                Some(u) => u.0 = tick,
                None => {
                    self.thumb_use.insert(key, (tick, 0));
                    self.thumb_cost += cost(0);
                }
            }
        }
        match self.map.get(&key) {
            Some(Slot::Ready(img)) => Some(img.clone()),
            Some(Slot::Pending) | Some(Slot::Failed) => None,
            None => {
                self.map.insert(key, Slot::Pending);
                self.pool.request(make());
                None
            }
        }
    }

    /// Drops least recently used thumbnails until under budget (cheap check; the sort only
    /// runs when over budget, and then frees a batch down to 90 % to avoid doing it per frame).
    fn evict_over_budget(&mut self) {
        if self.thumb_cost <= THUMB_BUDGET {
            return;
        }
        let target = THUMB_BUDGET / 10 * 9;
        let mut order: Vec<(u64, u64, usize)> = self.thumb_use.iter().map(|(k, (t, b))| (*k, *t, *b)).collect();
        order.sort_unstable_by_key(|(_, t, _)| *t);
        for (key, _, bytes) in order {
            if self.thumb_cost <= target {
                break;
            }
            self.thumb_cost -= cost(bytes);
            self.thumb_use.remove(&key);
            if let Some(Slot::Ready(img)) = self.map.remove(&key) {
                self.evicted.push(img);
            }
        }
    }

    /// Dropping the `Arc` is not enough: GPUI keeps an uploaded image in each window's sprite
    /// atlas until `drop_image`. Do it after the current window update (a window being updated
    /// is temporarily out of the app's window list, so an immediate call would miss it).
    fn release_evicted(&mut self, cx: &mut Context<Self>) {
        if self.evicted.is_empty() {
            return;
        }
        let imgs = std::mem::take(&mut self.evicted);
        cx.defer(move |cx| {
            for img in imgs {
                cx.drop_image(img, None);
            }
        });
    }

    /// Small shell icon for a row. Generic types are cached per extension, folders once.
    pub fn icon(&mut self, dir: &Path, e: &Entry, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let unique = !e.is_dir() && has_unique_icon(&e.ext);
        let key = if e.is_dir() {
            key_of(&[&"dir", &ICON_PX])
        } else if unique {
            key_of(&[&"path", &dir, &&*e.name, &ICON_PX])
        } else {
            key_of(&[&"ext", &&*e.ext, &ICON_PX])
        };
        let path = dir.join(&*e.name);
        // Per-file icons (exe/lnk/…) are unbounded in number: keep them in the LRU too.
        let r = self.lookup(key, || Request { key, path, size: ICON_PX, kind: Kind::Icon }, unique);
        self.evict_over_budget();
        self.release_evicted(cx);
        r
    }

    /// Large thumbnail (falls back to the large icon when the file has no thumbnail).
    /// The shell's picture for a parsing name (an app of "All apps": `shell:AppsFolder\<id>`).
    /// Asked as a thumbnail: inside smoothyfiles the icon of a Store app comes back as the shell's
    /// blank app placeholder, its thumbnail is the app's own logo.
    pub fn shell_icon(&mut self, parsing: &str, px: u32, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let key = key_of(&[&"shell", &parsing, &px]);
        let path = std::path::PathBuf::from(parsing);
        let r = self.lookup(key, || Request { key, path, size: px, kind: Kind::Thumbnail }, true);
        self.evict_over_budget();
        self.release_evicted(cx);
        r
    }

    pub fn thumbnail(&mut self, dir: &Path, e: &Entry, size: u32, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let wants_thumb = crate::icons::has_thumbnail(e);
        let per_file = wants_thumb || (!e.is_dir() && has_unique_icon(&e.ext));
        let key = if per_file {
            key_of(&[&"thumb", &dir, &&*e.name, &e.modified_ms, &size])
        } else if e.is_dir() {
            key_of(&[&"dirL", &size])
        } else {
            key_of(&[&"extL", &&*e.ext, &size])
        };
        let path = dir.join(&*e.name);
        let kind = if wants_thumb { Kind::Thumbnail } else { Kind::Icon };
        // every per-file picture (an exe's or a shortcut's icon too) is in the LRU, or browsing
        // folders of programs would keep them all until the app closes
        let r = self.lookup(key, || Request { key, path, size, kind }, per_file);
        self.evict_over_budget();
        self.release_evicted(cx);
        r
    }

    pub fn cancel_pending(&mut self) {
        self.pool.clear();
        let map = &mut self.map;
        map.retain(|_, s| !matches!(s, Slot::Pending));
        let mut freed = 0;
        self.thumb_use.retain(|k, (_, b)| {
            let keep = map.contains_key(k);
            if !keep {
                freed += cost(*b);
            }
            keep
        });
        self.thumb_cost -= freed;
    }
}
