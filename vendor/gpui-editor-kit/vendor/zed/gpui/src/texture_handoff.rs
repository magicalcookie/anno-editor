//! A texture another thread renders on GPUI's device, which GPUI draws in
//! the same frame (Linux, Windows and macOS). Added for gpui-editor-kit;
//! see `vendor/zed/PATCH.md`.
//!
//! Each frame that will draw it asks the producer for a frame as it starts,
//! before layout; the producer records and submits into its texture while
//! GPUI lays out and paints, and GPUI's renderer waits for that submit just
//! before it draws the texture. One device and one queue: submission order
//! is the GPU sync, so there is no second texture and no frame of latency.
//! A producer that misses the deadline leaves its last complete frame on
//! screen, and the window asks for another frame to show the late one.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a producer records with: GPUI's device and queue. Its texture
/// may have any format the sprite shader samples as float.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct HandoffGpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// A finished frame: the texture's view, the device it was made on, and the
/// part of it the frame covers from the top left corner, in device pixels.
/// The texture needs `TEXTURE_BINDING`.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct HandoffTexture {
    pub device: wgpu::Device,
    pub view: wgpu::TextureView,
    pub size: [u32; 2],
}

/// How the last composite went, for the producer's timing.
#[derive(Clone, Copy, Debug)]
pub struct Composite {
    /// Which window frame drew the texture, counted from 1 (`drawn`).
    pub index: u64,
    /// The producer frame it drew (0: none yet), and the frame asked for
    /// then.
    pub drew: u64,
    #[allow(missing_docs)]
    pub asked: u64,
    /// How long GPUI's thread waited for the producer before drawing.
    pub waited: Duration,
    /// How long the swapchain held GPUI's thread before the frame.
    pub surface_wait: Duration,
    /// When the wait ended and the draw was recorded.
    pub at: Instant,
}

/// The handshake between one producer and the windows that draw its
/// texture. Paint it with `Window::paint_gpu_texture`.
#[derive(Debug)]
pub struct TextureHandoff {
    state: Mutex<State>,
    changed: Condvar,
    deadline: Duration,
}

#[derive(Debug, Default)]
struct State {
    /// Frames asked for, and when the latest was.
    asked: u64,
    asked_at: Option<Instant>,
    /// The newest frame the producer submitted or passed over.
    answered: u64,
    /// The last texture it handed over, and the frame that drew it.
    texture: Option<(u64, HandoffTexture)>,
    gpu: Option<HandoffGpu>,
    /// A composite is between its wait and its submit: the producer must
    /// not submit, or its write would land before GPUI's read.
    reading: bool,
    /// A composite drew an older frame than the one asked for.
    late: bool,
    /// Window frames that drew the texture, and the asks there had been
    /// by the newest of them.
    drawn: u64,
    read_asked: u64,
    composite: Option<Composite>,
    closed: bool,
}

impl TextureHandoff {
    /// `deadline`: how long after a frame's ask GPUI waits for it at most.
    pub fn new(deadline: Duration) -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            deadline,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks the producer for a new frame. Windows ask once a frame for each
    /// texture they draw; a producer asks again when what the frame shows
    /// changed after the ask (the pane moved in layout).
    pub fn ask(&self) {
        let mut state = self.lock();
        state.asked += 1;
        state.asked_at = Some(Instant::now());
        self.changed.notify_all();
    }

    /// Whether the producer has handed over a frame yet.
    pub fn has_frame(&self) -> bool {
        self.lock().texture.is_some()
    }

    /// Frames asked for so far.
    pub fn asked(&self) -> u64 {
        self.lock().asked
    }

    /// Window frames that drew the texture so far, whether or not there
    /// was a frame to draw.
    pub fn drawn(&self) -> u64 {
        self.lock().drawn
    }

    /// The newest ask, if no window frame has drawn the texture since:
    /// the frame it is for is still to come.
    pub fn unread(&self) -> Option<u64> {
        let state = self.lock();
        (state.asked > state.read_asked).then_some(state.asked)
    }

    /// For the producer: waits until a frame is asked for that it has not
    /// answered and GPUI's device is known, then returns the newest such
    /// frame's number and the device. Older asks fold into it. `None` once
    /// closed.
    pub fn wait(&self) -> Option<(u64, HandoffGpu)> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if state.asked > state.answered
                && let Some(gpu) = &state.gpu
            {
                return Some((state.asked, gpu.clone()));
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// For the producer: runs `submit`, which submits frame `frame`'s
    /// commands, once no composite is reading the texture, and keeps the
    /// texture it returns (`None` leaves the last one on screen).
    pub fn submit(&self, frame: u64, submit: impl FnOnce() -> Option<HandoffTexture>) {
        let mut state = self.lock();
        while state.reading && !state.closed {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if let Some(texture) = submit() {
            state.texture = Some((frame, texture));
        }
        state.answered = state.answered.max(frame);
        self.changed.notify_all();
    }

    /// For the producer: how the last composite went, once.
    pub fn take_composite(&self) -> Option<Composite> {
        self.lock().composite.take()
    }

    /// Ends the producer: `wait` returns `None` from now on.
    pub fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// For GPUI's renderer, before it draws the texture: waits for the
    /// frame asked for until the deadline, and returns the texture to draw
    /// if it was made on `gpu`'s device. It does not wait when the producer
    /// has yet to see this device (the first frame, or a new device after a
    /// loss), or when it has not yet answered what the previous frame that
    /// drew it asked for, since it will not catch up. Every call
    /// must be followed by `end_read` once the frame is submitted.
    #[doc(hidden)]
    pub fn begin_read(
        &self,
        gpu: &HandoffGpu,
        surface_wait: Duration,
    ) -> Option<(wgpu::TextureView, [u32; 2])> {
        let started = Instant::now();
        let mut state = self.lock();
        let known = (state.gpu.as_ref()).is_some_and(|g| g.device == gpu.device);
        if !known {
            state.gpu = Some(gpu.clone());
            self.changed.notify_all();
        } else if state.answered >= state.read_asked
            && let Some(asked_at) = state.asked_at
        {
            let until = asked_at + self.deadline;
            while state.answered < state.asked && !state.closed {
                let Some(left) = until.checked_duration_since(Instant::now()) else {
                    break;
                };
                state = (self.changed.wait_timeout(state, left))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        // A closed handoff still draws its last frame (a pane kept on
        // screen after its producer ended), and no frame will follow it.
        state.late |= state.answered < state.asked && !state.closed;
        state.reading = true;
        state.drawn += 1;
        state.read_asked = state.asked;
        let texture = (state.texture.clone()).filter(|(_, t)| t.device == gpu.device);
        let at = Instant::now();
        state.composite = Some(Composite {
            index: state.drawn,
            drew: texture.as_ref().map_or(0, |(frame, _)| *frame),
            asked: state.asked,
            waited: at - started,
            surface_wait,
            at,
        });
        texture.map(|(_, t)| (t.view.clone(), t.size))
    }

    /// For GPUI's renderer: the frame that read the texture is submitted.
    #[doc(hidden)]
    pub fn end_read(&self) {
        self.lock().reading = false;
        self.changed.notify_all();
    }

    /// For the window, after presenting: whether a composite since the last
    /// call drew an older frame than asked, so another frame should show
    /// the late one.
    pub(crate) fn take_late(&self) -> bool {
        std::mem::take(&mut self.lock().late)
    }
}
