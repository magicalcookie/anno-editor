// Owns the memory report: resident size from /proc, and a sampling heap
// profiler that says which call sites hold the live heap.
//
// The profiler is the global allocator in tests and with the
// `heap-profile` feature; it records nothing until `start`. About one
// allocation per `RATE` bytes allocated keeps its call stack, weighted so
// the sum estimates the live heap (tcmalloc's scheme): allocations of
// `RATE` or more are always kept. `report` resolves the stacks and prints
// the largest owners.

/// Resident and peak resident set, its anonymous, file and shared-memory
/// shares and what is swapped out, in MiB, from /proc/self/status, with
/// glibc's count. Zeros where there is no /proc. Other processes' pressure
/// swaps pages out: RSS plus swap is what the process holds.
pub fn resident() -> Resident {
    Resident {
        malloc: malloc(),
        ..status()
    }
}

/// `resident` without glibc's count, which locks every arena.
fn status() -> Resident {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        (status.lines())
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
            .map_or(0., |kb| kb as f64 / 1024.)
    };
    Resident {
        rss: field("VmRSS:"),
        peak: field("VmHWM:"),
        anon: field("RssAnon:"),
        file: field("RssFile:"),
        shmem: field("RssShmem:"),
        swap: field("VmSwap:"),
        malloc: (0., 0.),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Resident {
    pub rss: f64,
    pub peak: f64,
    pub anon: f64,
    pub file: f64,
    /// The GPU driver's mapped buffers land here or in `file`.
    pub shmem: f64,
    pub swap: f64,
    /// `malloc()` when it was read.
    pub malloc: (f64, f64),
}

impl std::fmt::Display for Resident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (used, free) = self.malloc;
        write!(
            f,
            "RSS {:.0} MiB (anon {:.0}, file {:.0}, shmem {:.0}, swapped {:.0}, malloc {used:.0} + {free:.0} free), peak {:.0} MiB",
            self.rss, self.anon, self.file, self.shmem, self.swap, self.peak
        )
    }
}

/// glibc's own count: bytes handed out (Rust's and C libraries' alike,
/// the GPU driver's included), and free bytes it still holds, in MiB.
pub fn malloc() -> (f64, f64) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        #[repr(C)]
        struct Mallinfo2 {
            arena: usize,
            ordblks: usize,
            smblks: usize,
            hblks: usize,
            hblkhd: usize,
            usmblks: usize,
            fsmblks: usize,
            uordblks: usize,
            fordblks: usize,
            keepcost: usize,
        }
        unsafe extern "C" {
            fn mallinfo2() -> Mallinfo2;
        }
        // SAFETY: glibc's own call; it walks every arena under its locks.
        let m = unsafe { mallinfo2() };
        let mib = |b: usize| b as f64 / (1 << 20) as f64;
        (mib(m.uordblks + m.hblkhd), mib(m.fordblks))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    (0., 0.)
}

/// Starts the peak resident set over from the current one (Linux's
/// clear_refs 5), so a step's peak is its own and not an earlier one's.
#[cfg(any(test, feature = "heap-profile"))]
pub fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Hands freed heap back to the system now, on this thread, so `resident`
/// shows what is live rather than what glibc's arenas kept.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: glibc's own call, safe from any thread.
        unsafe { malloc_trim(0) };
    }
}

/// Samples the resident set every 5 ms on a thread until `stop`, keeping
/// the sample where it was highest and the live heap then: what a peak was
/// made of, which `peak` alone does not say.
#[cfg(any(test, feature = "heap-profile"))]
pub struct Watch {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<(Resident, f64, std::time::Duration)>,
}

#[cfg(any(test, feature = "heap-profile"))]
impl Watch {
    pub fn start() -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = stop.clone();
            move || {
                let start = std::time::Instant::now();
                let sample = || {
                    let r = resident();
                    (r, profile::mark(r.rss), start.elapsed())
                };
                let mut high = sample();
                while !stop.load(Relaxed) {
                    // glibc's count and the heap's only past 4 MiB more:
                    // mallinfo2 locks every arena the load allocates from.
                    if status().rss > high.0.rss + 4. {
                        high = sample();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                high
            }
        });
        Self { stop, thread }
    }

    /// The highest sample: resident set, the sampled live heap in MiB, and
    /// when it was taken after `start`.
    pub fn stop(self) -> (Resident, f64, std::time::Duration) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.join().expect("memory watch")
    }
}

#[cfg(any(test, feature = "heap-profile"))]
pub use profile::{Sampler, live, report, start};

#[cfg(any(test, feature = "heap-profile"))]
mod profile {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};

    /// Bytes allocated per kept stack, on average.
    const RATE: isize = 256 << 10;
    /// Frames kept per stack, innermost first.
    const DEPTH: usize = 40;
    /// Live sampled pointers, hashed: a free whose slot is 0 was never
    /// sampled and skips the lock. Saturating counts, so a collision only
    /// costs a lookup.
    const FILTER: usize = 1 << 16;

    static ON: AtomicBool = AtomicBool::new(false);
    static FILTER_BITS: [AtomicU8; FILTER] = [const { AtomicU8::new(0) }; FILTER];
    static TABLE: Mutex<Option<Table>> = Mutex::new(None);

    thread_local! {
        static COUNTDOWN: Cell<isize> = const { Cell::new(RATE) };
        /// Inside the profiler on this thread: its own allocations pass
        /// straight through.
        static BUSY: Cell<bool> = const { Cell::new(false) };
    }

    #[derive(Default)]
    struct Table {
        stacks: Vec<[usize; DEPTH]>,
        by_stack: HashMap<[usize; DEPTH], u32>,
        /// Live sampled allocation → (stack, weight in bytes).
        live: HashMap<usize, (u32, usize)>,
        /// Live bytes per stack, and their sum.
        per_stack: Vec<usize>,
        total: usize,
        /// `per_stack` when `total` was highest.
        peak: Vec<usize>,
        peak_total: usize,
        /// Highest `total` since `live` last asked.
        high: usize,
        /// `per_stack` at the highest resident set a `Watch` saw, and that.
        at_rss: (f64, Vec<usize>),
    }

    impl Table {
        fn add(&mut self, ptr: usize, ips: [usize; DEPTH], weight: usize) {
            let stack = match self.by_stack.get(&ips) {
                Some(&s) => s,
                None => {
                    let s = self.stacks.len() as u32;
                    self.stacks.push(ips);
                    self.by_stack.insert(ips, s);
                    self.per_stack.push(0);
                    s
                }
            };
            if let Some((old, w)) = self.live.insert(ptr, (stack, weight)) {
                self.per_stack[old as usize] -= w;
                self.total -= w;
            } else {
                let f = &FILTER_BITS[slot(ptr)];
                let _ = f.try_update(Relaxed, Relaxed, |c| c.checked_add(1));
            }
            self.per_stack[stack as usize] += weight;
            self.total += weight;
            self.high = self.high.max(self.total);
            // A copy per 16 MB of new peak: a few hundred over a load.
            if self.total > self.peak_total + (16 << 20) || self.peak.is_empty() {
                self.peak.clone_from(&self.per_stack);
                self.peak_total = self.total;
            }
        }

        fn remove(&mut self, ptr: usize) {
            if let Some((stack, w)) = self.live.remove(&ptr) {
                self.per_stack[stack as usize] -= w;
                self.total -= w;
                // A saturated count stays: its slot always takes the lock.
                let f = &FILTER_BITS[slot(ptr)];
                let _ = f.try_update(Relaxed, Relaxed, |c| (c < u8::MAX).then(|| c - 1));
            }
        }
    }

    fn slot(ptr: usize) -> usize {
        (ptr >> 4).wrapping_mul(0x9e37_79b9) >> 7 & (FILTER - 1)
    }

    pub struct Sampler;

    impl Sampler {
        fn note_alloc(ptr: *mut u8, size: usize) {
            if ptr.is_null() || !ON.load(Relaxed) || BUSY.get() {
                return;
            }
            let left = COUNTDOWN.get() - size as isize;
            if left > 0 {
                COUNTDOWN.set(left);
                return;
            }
            COUNTDOWN.set(RATE);
            BUSY.set(true);
            let mut ips = [0usize; DEPTH];
            let mut n = 0;
            // SAFETY: the stack is walked on this thread only; BUSY keeps
            // the unwinder's own allocations out of the profiler.
            unsafe {
                backtrace::trace_unsynchronized(|f| {
                    ips[n] = f.ip() as usize;
                    n += 1;
                    n < DEPTH
                })
            };
            if let Ok(mut table) = TABLE.lock() {
                let t = table.get_or_insert_with(Table::default);
                t.add(ptr as usize, ips, size.max(RATE as usize));
            }
            BUSY.set(false);
        }

        fn note_free(ptr: *mut u8) {
            if FILTER_BITS[slot(ptr as usize)].load(Relaxed) == 0 || BUSY.get() {
                return;
            }
            BUSY.set(true);
            if let Ok(mut table) = TABLE.lock()
                && let Some(t) = table.as_mut()
            {
                t.remove(ptr as usize);
            }
            BUSY.set(false);
        }
    }

    // SAFETY: every call forwards to the system allocator unchanged; the
    // bookkeeping never touches the memory handed out.
    unsafe impl GlobalAlloc for Sampler {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(layout) };
            Self::note_alloc(p, layout.size());
            p
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc_zeroed(layout) };
            Self::note_alloc(p, layout.size());
            p
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            Self::note_free(ptr);
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            Self::note_free(ptr);
            let p = unsafe { System.realloc(ptr, layout, new_size) };
            // Growth counts as a fresh allocation of the new size: a Vec
            // grown to 100 MB is sampled by its final size, not its steps.
            Self::note_alloc(p, new_size);
            p
        }
    }

    /// Starts keeping stacks of new allocations. What was allocated before
    /// stays unseen, so start before the work to be measured.
    pub fn start() {
        ON.store(true, Relaxed);
    }

    /// The estimated live heap now, in MiB, kept by owner for `report`
    /// when `rss` is the highest resident set marked yet.
    pub(super) fn mark(rss: f64) -> f64 {
        BUSY.set(true);
        let total = TABLE.lock().ok().and_then(|mut table| {
            let t = table.as_mut()?;
            if rss > t.at_rss.0 {
                t.at_rss = (rss, t.per_stack.clone());
            }
            Some(t.total)
        });
        BUSY.set(false);
        total.unwrap_or(0) as f64 / (1 << 20) as f64
    }

    /// The estimated live heap now and its highest since the last call, in
    /// MiB: one step's peak, where `report` has the whole run's.
    pub fn live() -> (f64, f64) {
        BUSY.set(true);
        let (now, high) = TABLE
            .lock()
            .ok()
            .and_then(|mut table| {
                let t = table.as_mut()?;
                Some((t.total, std::mem::replace(&mut t.high, t.total)))
            })
            .unwrap_or_default();
        BUSY.set(false);
        let mib = |b: usize| b as f64 / (1 << 20) as f64;
        (mib(now), mib(high))
    }

    /// Prints the estimated live heap and its `top` largest owners, now and
    /// when it peaked: by the innermost frame of the editor's own crates,
    /// then whole stacks with their own frames. Resolving symbols loads the
    /// debug info, hundreds of MB: measure RSS before calling this.
    pub fn report(top: usize) {
        ON.store(false, Relaxed);
        BUSY.set(true);
        let (stacks, now, peak, at_rss) = {
            let table = TABLE.lock().unwrap();
            let Some(t) = table.as_ref() else {
                BUSY.set(false);
                return;
            };
            let (stacks, now) = (t.stacks.clone(), t.per_stack.clone());
            (stacks, now, t.peak.clone(), t.at_rss.clone())
        };
        BUSY.set(false);
        let mut names: HashMap<usize, Vec<String>> = HashMap::new();
        print("at its peak", &stacks, &peak, top, &mut names);
        if !at_rss.1.is_empty() {
            let title = format!("at the highest RSS ({:.0} MiB)", at_rss.0);
            print(&title, &stacks, &at_rss.1, top, &mut names);
        }
        print("now", &stacks, &now, top, &mut names);
    }

    fn print(
        title: &str,
        stacks: &[[usize; DEPTH]],
        live: &[usize],
        top: usize,
        names: &mut HashMap<usize, Vec<String>>,
    ) {
        let total: usize = live.iter().sum();
        eprintln!("heap {title}: {:.0} MiB live (sampled)", mib(total));
        // Frames of our crates, innermost first, generic helpers kept.
        let mut own = |ips: &[usize; DEPTH]| -> Vec<String> {
            let mut frames = Vec::new();
            for &ip in ips.iter().filter(|&&ip| ip != 0) {
                let names = names.entry(ip).or_insert_with(|| {
                    let mut out = Vec::new();
                    backtrace::resolve(ip as *mut _, |sym| {
                        let Some(name) = sym.name() else { return };
                        let name = format!("{name:#}");
                        let ours = [
                            "anno_",
                            "anno117",
                            "anno1800",
                            "anno2070",
                            "gpui_editor_kit",
                        ]
                        .iter()
                        .any(|c| name.starts_with(c) || name.starts_with(&format!("<{c}")));
                        if ours && !name.contains("heap::profile") {
                            let line = sym.lineno().map_or(String::new(), |l| format!(":{l}"));
                            out.push(format!("{name}{line}"));
                        }
                    });
                    out
                });
                frames.extend(names.iter().cloned());
            }
            frames
        };
        let mut order: Vec<usize> = (0..live.len()).filter(|&s| live[s] > 0).collect();
        order.sort_by_key(|&s| std::cmp::Reverse(live[s]));
        let mut sites: HashMap<String, usize> = HashMap::new();
        let mut resolved = Vec::new();
        for &s in &order {
            let frames = own(&stacks[s]);
            let site = frames
                .first()
                .cloned()
                .unwrap_or_else(|| "(outside)".into());
            *sites.entry(site).or_default() += live[s];
            resolved.push((live[s], frames));
        }
        let mut sites: Vec<_> = sites.into_iter().collect();
        sites.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
        eprintln!("heap {title} by innermost own frame:");
        for (site, bytes) in sites.iter().take(top) {
            eprintln!("  {:>7.1} MiB  {site}", mib(*bytes));
        }
        eprintln!("heap {title} by stack:");
        for (bytes, frames) in resolved.iter().take(top) {
            eprintln!("  {:>7.1} MiB", mib(*bytes));
            for f in frames.iter().take(10) {
                eprintln!("      {f}");
            }
        }
    }

    fn mib(b: usize) -> f64 {
        b as f64 / (1 << 20) as f64
    }
}
