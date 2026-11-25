// Owns which mip levels of the arena's placement textures stay on the GPU.
// Each view's cull asks, per texture slot, for the finest level a placement
// it passes could sample (cull.wgsl `want_texels`), the shadow maps' culls
// likewise; the answers arrive a frame later (`Wants`). A texture gains one
// level a step while asked for finer than it holds, from texels in memory
// or read again on a thread, and loses levels nobody asked for over the
// last HOLD to twice that. Each change makes its device texture again, the
// levels it keeps copied across on the GPU, and the arena binds it anew:
// shaders sample a smaller texture with the same UVs, so nothing reads a
// level that is not there and the levels that are sample as before.
use super::LockAny as _;
use super::{Budget, Image, Live, MAX_CASCADES, RELEASED, StreamedTexture, View};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Whether placement textures keep only the levels views sample. Off,
/// every one is asked for whole: for comparing images and memory against.
pub static TEXTURE_RESIDENCY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Sections of a view's need buffer: the view's cull, the shadow map's,
/// then each cascade's. Each keeps its last answer until its cull runs
/// again: a kept view or shadow map still samples what it asked for.
pub(super) const SECTIONS: u32 = 2 + MAX_CASCADES as u32;
/// Words per section: the arena binds at most this many textures
/// (`Kit::arena`).
pub(super) const SLOTS: u32 = 2048;
/// Levels nobody asks for stay this long at least, twice at most: a camera
/// orbiting back and forth keeps what it keeps returning to.
const HOLD: Duration = Duration::from_secs(2);
/// A managed texture's top level keeps its smaller side at least this
/// large: 64² of BC7 with its mips is 5.4 KB, and a placement coming into
/// view shows that at once.
const FLOOR_SIDE: u32 = 64;

/// Texel reads from disk for every renderer in the process: a fixed set of
/// threads, and at most that many reads waiting or running, so a frame that
/// finds no room asks again on the next with the views' latest wants.
///
/// Hosts read placement textures' fine levels only once views ask
/// (`Releasable::from_tail`), so an island's opening view reads hundreds: at
/// 4 at once the continental island took 117 frames to full detail, at 16
/// 56. Half the cores, 4 to 16: a read is a page-cache copy, and an inflate
/// for compressed entries, and the frame and the model loads need the rest.
/// At 16 per renderer, tests drawing islands side by side could run
/// hundreds of reads at once: a ground test missed its 20 s upload limit
/// in 3 of 4 full test runs, and in none of 4 with one pool. What is held
/// is what is growing, whatever the count.
struct Reads {
    jobs: std::sync::Mutex<std::collections::VecDeque<Box<dyn FnOnce() + Send>>>,
    ready: std::sync::Condvar,
    /// Reads queued or running.
    taken: AtomicUsize,
    size: usize,
}

impl Reads {
    fn get() -> &'static Reads {
        static READS: std::sync::OnceLock<&'static Reads> = std::sync::OnceLock::new();
        READS.get_or_init(|| {
            let cores = std::thread::available_parallelism().map_or(8, |n| n.get());
            let reads: &'static Reads = Box::leak(Box::new(Reads {
                jobs: Default::default(),
                ready: Default::default(),
                taken: AtomicUsize::new(0),
                size: (cores / 2).clamp(4, 16),
            }));
            for _ in 0..reads.size {
                std::thread::Builder::new()
                    .name("texture reads".into())
                    .spawn(|| reads.work())
                    .expect("texture read thread");
            }
            reads
        })
    }

    /// Queues `job` if a read is free; false when all are taken.
    fn try_run(&self, job: impl FnOnce() + Send + 'static) -> bool {
        let take = |n: usize| (n < self.size).then_some(n + 1);
        if self
            .taken
            .try_update(Ordering::AcqRel, Ordering::Acquire, take)
            .is_err()
        {
            return false;
        }
        self.jobs.lock_any().push_back(Box::new(job));
        self.ready.notify_one();
        true
    }

    fn work(&self) {
        loop {
            let job = {
                let mut jobs = self.jobs.lock_any();
                loop {
                    match jobs.pop_front() {
                        Some(job) => break job,
                        None => jobs = self.ready.wait(jobs).unwrap(),
                    }
                }
            };
            job();
            self.taken.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// A want: 32 less the finest level asked for, 0 for none (cull.wgsl).
fn level(want: u8) -> Option<u32> {
    (want > 0).then(|| 32u32.saturating_sub(u32::from(want)))
}

/// A managed texture's residency (`Live::managed`).
pub(super) struct Managed {
    /// The layout level the texture's top may go down to.
    pub coarsest: u32,
    /// The finest want in each half of the hold window.
    recent: [u8; 2],
    /// The hold epoch `recent[1]` belongs to.
    epoch: u64,
    /// The finer levels asked for, filling a device texture of their own
    /// until it takes the place of the live one (`grow`).
    upgrade: Option<Upgrade>,
    /// A read of released texels on a thread: 1 running, 2 done, 3 failed.
    reload: Option<Arc<AtomicU8>>,
    /// The texture last gained a level, or was made: its texels go HOLD
    /// after, unless it keeps growing.
    grown: Instant,
}

/// A device texture filling with a managed texture's finer levels: its
/// level 0 holds layout level `top`, and layout level `level` fills from
/// band row `row`; the coarser ones it gets from the live texture.
struct Upgrade {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    top: u32,
    level: u32,
    row: u32,
}

/// The coarsest level a placement texture of `levels` levels from
/// `width` x `height` keeps on the GPU, and starts from: hosts need hold no
/// finer one until views ask (`texture::Releasable::from_tail`). Taken as
/// if the device held blocks; one that decodes keeps the same or coarser.
pub fn floor_level(width: u32, height: u32, levels: usize) -> usize {
    floor(width, height, levels as u32, true) as usize
}

/// `block`: the device holds blocks, so a top level must keep both sides
/// multiples of 4 (wgpu's base size rule).
fn floor(width: u32, height: u32, levels: u32, block: bool) -> u32 {
    let fits = |l: u32| {
        let (w, h) = (width >> l, height >> l);
        w.min(h) >= FLOOR_SIDE && (!block || (w % 4 == 0 && h % 4 == 0))
    };
    (0..levels).rev().find(|&l| fits(l)).unwrap_or(0)
}

impl Managed {
    pub fn new(layout: &super::Layout, block: bool) -> Self {
        let coarsest = floor(layout.width, layout.height, layout.levels, block);
        Self {
            coarsest,
            recent: [0; 2],
            epoch: 0,
            upgrade: None,
            reload: None,
            grown: Instant::now(),
        }
    }
}

/// What a view's culls last asked of the arena's textures: per section a
/// want per slot.
#[derive(Default)]
pub(super) struct Wants {
    arena: u64,
    slots: usize,
    values: Vec<u8>,
    /// The view the latest answer culled.
    view: Option<View>,
    /// Changed since the last step took them.
    pub fresh: bool,
}

impl Wants {
    /// Takes a readback of `sections` (a bit each, in order, `slots` words
    /// apiece) the culls of arena `arena` wrote, looking through `view`.
    pub fn take(&mut self, arena: u64, slots: usize, sections: u32, view: View, words: &[u32]) {
        if self.arena != arena || self.slots != slots {
            *self = Self {
                arena,
                slots,
                values: vec![0; SECTIONS as usize * slots],
                view: None,
                fresh: true,
            };
        }
        self.view = Some(view);
        let written = (0..SECTIONS).filter(|s| sections & 1 << s != 0);
        for (section, words) in written.zip(words.chunks(slots)) {
            let at = section as usize * slots;
            for (v, &w) in self.values[at..at + slots].iter_mut().zip(words) {
                let w = w.min(32) as u8;
                self.fresh |= *v != w;
                *v = w;
            }
        }
    }
}

/// The arena's residency across frames.
#[derive(Default)]
pub(super) struct Residency {
    started: Option<Instant>,
    /// The arena some view has answered for since it landed.
    heard: Option<u64>,
    /// Textures asked for finer than they hold, as of the last step.
    pub wanted: usize,
    /// Levels wait out the hold window to go: frames should keep coming.
    pub due: bool,
    /// The arena every slot of which holds what impostor bakes sample
    /// (`Arena::floors`).
    pub floors_met: Option<u64>,
    /// Tests: levels nobody asks for go at once, or every level stays.
    pub hold: Option<Duration>,
    pub whole: bool,
}

/// What a step changed.
#[derive(Default)]
pub(super) struct Stepped {
    /// Some device texture was made again: the arena binds anew.
    pub rebind: bool,
    /// A texture gained a level views had asked for: what they drew lacked it.
    pub grew: bool,
    /// ... and a shadow map had asked for it too.
    pub shadows: bool,
}

impl Wants {
    /// Whether these are `arena`'s wants as seen through `view`.
    pub fn answers(&self, arena: u64, view: Option<View>) -> bool {
        self.arena == arena && self.view.is_some() && self.view == view
    }
}

impl Residency {
    /// Forgets every arena heard of, for a new device's; the tests'
    /// settings stay.
    pub fn restart(&mut self) {
        *self = Self {
            hold: self.hold,
            whole: self.whole,
            ..Self::default()
        };
    }

    /// One frame's residency for the textures `slots` binds (`None`
    /// white), as `views` (their `Wants`) and `floors` ask: levels gained
    /// within `budget`, levels let go that waited out the hold.
    #[allow(clippy::too_many_arguments)]
    pub fn step<'w>(
        &mut self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        arena: u64,
        slots: &[Option<Arc<StreamedTexture>>],
        floors: &[u8],
        views: impl Iterator<Item = &'w mut Wants>,
        budget: &mut Budget,
    ) -> Stepped {
        let now = Instant::now();
        let started = *self.started.get_or_insert(now);
        let hold = self.hold.unwrap_or(HOLD);
        // Without a hold every step is an epoch of its own.
        let epoch = match hold.as_nanos() {
            0 => u64::MAX,
            n => ((now - started).as_nanos() / n) as u64,
        };
        // The finest want per slot over every view and section, and over
        // the shadow sections alone.
        let n = slots.len();
        let mut want = floors.to_vec();
        want.resize(n, 0);
        let mut shadow = vec![0u8; n];
        for wants in views {
            if wants.arena != arena || wants.slots != n {
                continue;
            }
            wants.fresh = false;
            self.heard = Some(arena);
            for (s, values) in wants.values.chunks(n).enumerate() {
                for (i, &v) in values.iter().enumerate() {
                    want[i] = want[i].max(v);
                    if s > 0 {
                        shadow[i] = shadow[i].max(v);
                    }
                }
            }
        }
        let whole = self.whole || !TEXTURE_RESIDENCY.load(Ordering::Relaxed);
        let heard = self.heard == Some(arena);
        let mut stepped = Stepped::default();
        let mut grow: Vec<(u32, usize, u32)> = Vec::new();
        self.due = false;
        let mut floors_met = true;
        for (i, t) in slots.iter().enumerate() {
            let Some(t) = t else { continue };
            let mut live = t.live.lock_any();
            // Only once the first fill is in: until then its levels are
            // not all there to copy.
            if (live.progress.0 as usize) < t.images.len() {
                floors_met = false;
                continue;
            }
            let base = live.base;
            let Some(m) = live.managed.as_mut() else {
                continue;
            };
            let want = if whole { 32 } else { want[i] };
            if epoch != m.epoch || epoch == u64::MAX {
                m.recent = if m.epoch.checked_add(1) == Some(epoch) {
                    [m.recent[1], 0]
                } else {
                    [0; 2]
                };
                m.epoch = epoch;
            }
            m.recent[1] = m.recent[1].max(want);
            let coarsest = m.coarsest;
            let clamp = |want: u8| level(want).map_or(coarsest, |l| l.min(coarsest));
            let finest = clamp(want);
            let kept = clamp(m.recent[0].max(m.recent[1]));
            if floors.get(i).is_some_and(|&f| clamp(f) < base) {
                floors_met = false;
            }
            let failed = (m.reload.as_ref()).is_some_and(|r| r.load(Ordering::Acquire) == 3);
            if finest < base && !failed {
                // Larger gaps first: the blurriest textures sharpen first.
                grow.push((base - finest, i, finest));
                continue;
            }
            m.upgrade = None;
            if kept > base {
                drop(live);
                t.shrink(gpu, encoder, kept);
                stepped.rebind = true;
                continue;
            }
            if finest > base {
                // Asked for less lately; goes once the hold runs out.
                self.due = true;
            }
            // Texels are read again to grow; between growth spurts they go.
            // Not under a read on its thread, which holds their lock: the
            // frame would wait on the disk.
            let reading = (m.reload.as_ref()).is_some_and(|r| r.load(Ordering::Acquire) == 1);
            if heard && now - m.grown >= hold && !reading {
                m.reload = None;
                if let Image::Releasable(r) = &t.images[0]
                    && r.loaded().is_some()
                {
                    r.release();
                    RELEASED.store(true, Ordering::Relaxed);
                }
            }
        }
        self.wanted = grow.len();
        self.floors_met = floors_met.then_some(arena);
        grow.sort_unstable_by_key(|&(gap, ..)| std::cmp::Reverse(gap));
        for &(_, i, finest) in &grow {
            if budget.spent() {
                break;
            }
            let t = slots[i].as_ref().expect("grows a texture");
            if t.grow(gpu, encoder, budget, finest) {
                stepped.rebind = true;
                stepped.grew = true;
                let base = t.live.lock_any().base;
                // The shadow maps drew without the level they asked for.
                stepped.shadows |= level(shadow[i]).is_some_and(|l| l <= base);
            }
        }
        stepped
    }
}

impl StreamedTexture {
    /// Fills the next finer level within `budget`, and once it is in,
    /// takes the texture with it in place of the live one. True when it
    /// did. Texels not held are read again first (`Reads`), from `finest`,
    /// the finest level views ask for, down.
    /// Fills every level from `finest`, the finest views ask for, to the
    /// live texture's within `budget`, and once they are in, takes the
    /// texture with them in place of the live one. True when it did.
    /// Texels not held are read again first (`Reads`); once their levels
    /// are on the GPU they go. A level a frame, and texels held out the
    /// hold, the continental island's opening view took 30 frames to its
    /// detail and held 200 MB of texels at its peak.
    fn grow(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        budget: &mut Budget,
        finest: u32,
    ) -> bool {
        let mut live = self.live.lock_any();
        let Live { base, managed, .. } = &mut *live;
        let (base, m) = (*base, managed.as_mut().expect("managed"));
        // A fill under way keeps its top: views asking finer meanwhile get
        // the next.
        let top = m.upgrade.as_ref().map_or(finest.min(base - 1), |u| u.top);
        let skip = self.layout.skips[0];
        let held;
        // Layout level `l`'s texels: in the image, or in what is held.
        fn level_data<'a>(
            image: &'a Image,
            skip: u32,
            held: &'a Option<Arc<crate::texture::Compressed>>,
            l: u32,
        ) -> Option<&'a [u8]> {
            match image {
                Image::Compressed(c) => Some(c.mips[(skip + l) as usize].as_slice()),
                Image::Releasable(r) => r.level(held.as_deref()?, (skip + l) as usize),
                Image::Rgba(_) => None,
            }
        }
        let image = &self.images[0];
        match &self.images[0] {
            Image::Releasable(r) => {
                held = r.loaded();
                if level_data(image, skip, &held, top).is_none() {
                    let reading =
                        (m.reload.as_ref()).is_some_and(|r| r.load(Ordering::Acquire) == 1);
                    if !reading {
                        let state = Arc::new(AtomicU8::new(1));
                        let (r, done) = (r.clone(), state.clone());
                        let first = (skip + top) as usize;
                        let queued = Reads::get().try_run(move || {
                            let ok = r.texels_from(first).is_some();
                            done.store(if ok { 2 } else { 3 }, Ordering::Release);
                        });
                        if queued {
                            m.reload = Some(state);
                        }
                    }
                    return false;
                }
            }
            // One level only: never managed.
            Image::Rgba(_) => return false,
            Image::Compressed(_) => held = None,
        }
        let mut up = m.upgrade.take().unwrap_or_else(|| {
            let (texture, view) = self.shape().create(&gpu.device, top);
            Upgrade {
                texture,
                view,
                top,
                level: top,
                row: 0,
            }
        });
        while up.level < base && !budget.spent() {
            let Some(data) = level_data(image, skip, &held, up.level) else {
                // Let go since: the next call reads them again.
                m.upgrade = Some(up);
                return false;
            };
            let rows = self.rows(up.level);
            while up.row < rows && !budget.spent() {
                let into = up.level - up.top;
                up.row += self
                    .band(
                        &gpu.queue,
                        &up.texture,
                        into,
                        0,
                        up.level,
                        up.row,
                        data,
                        budget,
                    )
                    .0;
            }
            if up.row >= rows {
                (up.level, up.row) = (up.level + 1, 0);
            }
        }
        if up.level < base {
            m.upgrade = Some(up);
            return false;
        }
        m.grown = Instant::now();
        self.copy_levels(encoder, &live.texture, base, &up.texture, up.top);
        (live.texture, live.view, live.base) = (up.texture, up.view, up.top);
        // On the GPU now; a finer level asked for later reads them again.
        if let Image::Releasable(r) = &self.images[0] {
            drop(held);
            r.release();
            RELEASED.store(true, Ordering::Relaxed);
        }
        true
    }

    /// Makes the device texture again from layout level `to` down, coarser
    /// than it is, copying the levels it keeps.
    fn shrink(&self, gpu: &super::Gpu, encoder: &mut wgpu::CommandEncoder, to: u32) {
        let mut live = self.live.lock_any();
        let (texture, view) = self.shape().create(&gpu.device, to);
        self.copy_levels(encoder, &live.texture, live.base, &texture, to);
        (live.texture, live.view, live.base) = (texture, view, to);
    }

    /// Copies every layout level both hold from `from` (level 0 at layout
    /// level `from_base`) to `to` (at `to_base`).
    fn copy_levels(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        from: &wgpu::Texture,
        from_base: u32,
        to: &wgpu::Texture,
        to_base: u32,
    ) {
        let (bw, bh) = self.format.block_dimensions();
        for level in from_base.max(to_base)..self.layout.levels {
            // Blocks cover whole 4x4 texels, also past a small mip's edge.
            let w = (self.layout.width >> level).max(1).next_multiple_of(bw);
            let h = (self.layout.height >> level).max(1).next_multiple_of(bh);
            let at = |texture, mip_level| wgpu::TexelCopyTextureInfo {
                texture,
                mip_level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            };
            encoder.copy_texture_to_texture(
                at(from, level - from_base),
                at(to, level - to_base),
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// The layout level the device texture's top holds.
    #[cfg(test)]
    pub(super) fn resident_base(&self) -> u32 {
        self.live.lock_any().base
    }
}
