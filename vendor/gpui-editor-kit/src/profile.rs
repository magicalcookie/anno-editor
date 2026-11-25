// Opt-in timing spans for finding where frames go. Off until the app calls
// `enable`; a disabled span costs one atomic load. Every 2 s the spans seen
// since the last report print to stderr: count, mean and worst.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
/// Per span name since the last report: count, total, worst.
type Spans = BTreeMap<&'static str, (u32, Duration, Duration)>;
static STATS: Mutex<Option<(Instant, Spans)>> = Mutex::new(None);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
    // GPUI's own draw phases, as spans named "gpui …": a window rebuild's
    // layout and paint happen there, outside the host's spans.
    gpui::set_draw_timer(add);
}

/// Whether spans are on: hosts print their own measurements only then.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Times until dropped: `let _span = profile::span("gpu draw");`.
pub fn span(name: &'static str) -> Option<Span> {
    ENABLED
        .load(Ordering::Relaxed)
        .then(|| Span(name, Instant::now()))
}

pub struct Span(&'static str, Instant);

/// Takes what was gathered since the last report or take, by span name:
/// count and total, slowest first. For benchmarks that look at one frame.
pub fn take() -> Vec<(&'static str, u32, Duration)> {
    let Some((_, spans)) = STATS.lock().unwrap().take() else {
        return Vec::new();
    };
    let mut out: Vec<_> = (spans.into_iter())
        .map(|(name, (n, total, _))| (name, n, total))
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out
}

/// Prints and clears what was gathered so far, without waiting for the
/// 2 s report: for benchmarks.
pub fn report() {
    if let Some((since, spans)) = STATS.lock().unwrap().take() {
        print(since, &spans);
    }
}

fn print(since: Instant, spans: &Spans) {
    let secs = since.elapsed().as_secs_f32();
    for (name, (n, total, worst)) in spans {
        eprintln!(
            "profile {name}: {n} in {secs:.1} s, mean {:?}, worst {worst:?}",
            *total / *n
        );
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        add(self.0, self.1.elapsed());
    }
}

/// Counts `took` under `name` as a span would: for times measured
/// elsewhere, such as a pass's GPU time read back from timestamps.
pub fn add(name: &'static str, took: Duration) {
    if !enabled() {
        return;
    }
    let mut stats = STATS.lock().unwrap();
    let (since, spans) = stats.get_or_insert_with(|| (Instant::now(), BTreeMap::new()));
    let (n, total, worst) = spans.entry(name).or_default();
    *n += 1;
    *total += took;
    *worst = (*worst).max(took);
    if since.elapsed() >= Duration::from_secs(2) {
        print(*since, spans);
        *stats = None;
    }
}
