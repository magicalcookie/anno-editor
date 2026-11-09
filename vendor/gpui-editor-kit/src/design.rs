// A 2D design surface: a fixed-size page shown with pan and zoom, and moving
// or resizing one rectangle by its body or eight handles, in design units.
// UI layouts, sprite sheets and 2D maps all edit rectangles on a page this
// way. Drawing the items stays with the host; positions come from here.
use crate::theme;
use gpui::{prelude::*, *};

/// Handle squares on the selected rectangle, screen pixels.
pub const HANDLE: f32 = 8.;

/// What a drag on a rectangle changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Move,
    /// One of eight resize handles, clockwise from the top-left corner:
    /// 0 top-left, 1 top, 2 top-right, 3 right, 4 bottom-right, 5 bottom,
    /// 6 bottom-left, 7 left.
    Resize(usize),
}

// Handle positions as fractions of the rectangle, in `Resize` order.
const SPOTS: [(f32, f32); 8] = [
    (0., 0.),
    (0.5, 0.),
    (1., 0.),
    (1., 0.5),
    (1., 1.),
    (0.5, 1.),
    (0., 1.),
    (0., 0.5),
];

/// A finished move or resize of `item`, as x, y, width, height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edit<K> {
    pub item: K,
    pub before: [f32; 4],
    pub after: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
enum Drag<K> {
    Pan(Point<Pixels>),
    Edit {
        item: K,
        handle: Handle,
        start: Point<Pixels>,
        rect: [f32; 4],
    },
}

/// View and drag state of one page. `K` identifies an editable rectangle.
/// Screen positions are relative to the pane's origin, so items can be
/// placed as absolutely positioned children of the pane.
#[derive(Clone, Debug)]
pub struct Design<K> {
    /// Screen pixels per design unit.
    pub zoom: f32,
    /// Screen position of the page's top-left corner.
    pub pan: [f32; 2],
    /// Page size in design units, e.g. the screen a UI was authored for.
    pub size: [f32; 2],
    drag: Option<Drag<K>>,
    /// The pane and page size the view was last fitted to; `None` until
    /// it fits. A new page size (a document that loads after the first
    /// layout) fits again unless the user has moved the view.
    fitted: Option<[f32; 4]>,
    /// The user has not moved the view since it fitted.
    auto: bool,
    /// What a fit frames, in design units; the whole page when `None`. A
    /// screen whose widgets fill a strip of the page opens on the strip.
    pub region: Option<[f32; 4]>,
    /// Pane pixels at the top that controls cover (a toolbar over the
    /// canvas); a fit centres the page in the rest.
    pub top_inset: f32,
    /// The same at the bottom (a hint bar).
    pub bottom_inset: f32,
}

impl<K> Default for Design<K> {
    fn default() -> Self {
        Self {
            zoom: 0.5,
            pan: [20., 20.],
            size: [1920., 1080.],
            drag: None,
            fitted: None,
            auto: true,
            region: None,
            top_inset: 0.,
            bottom_inset: 0.,
        }
    }
}

const ZOOM: std::ops::RangeInclusive<f32> = 0.05..=16.;

impl<K: Copy + PartialEq> Design<K> {
    pub fn to_screen(&self, x: f32, y: f32) -> Point<Pixels> {
        point(
            px(self.pan[0] + x * self.zoom),
            px(self.pan[1] + y * self.zoom),
        )
    }

    pub fn to_design(&self, p: Point<Pixels>) -> [f32; 2] {
        [
            (f32::from(p.x) - self.pan[0]) / self.zoom,
            (f32::from(p.y) - self.pan[1]) / self.zoom,
        ]
    }

    /// Fits the page into a pane of `size` with a 20 px margin. Call it
    /// every layout: it fits on the first real size after `refit`, and again
    /// whenever the pane changes size until the user zooms or pans (a pane
    /// that opens while the dock settles would otherwise keep the first,
    /// wrong size). Returns true when the view moved. Called from a
    /// prepaint, the frame is already built at the old zoom and a notify
    /// inside the draw is dropped: redraw from `window.on_next_frame`.
    pub fn fit(&mut self, size: Size<Pixels>) -> bool {
        let (w, h) = (f32::from(size.width), f32::from(size.height));
        let [x, y, rw, rh] = self.region.unwrap_or([0., 0., self.size[0], self.size[1]]);
        let key = [w, h, rw, rh];
        if w <= 1. || self.fitted == Some(key) || (self.fitted.is_some() && !self.auto) {
            return false;
        }
        self.fitted = Some(key);
        // A region gets a wider margin, so the page edge shows it is part
        // of a page.
        let margin = if self.region.is_some() { 80. } else { 40. };
        let free = (h - self.top_inset - self.bottom_inset).max(1.);
        self.zoom = ((w - margin) / rw.max(1.))
            .min((free - margin) / rh.max(1.))
            .clamp(*ZOOM.start(), *ZOOM.end());
        self.pan = [
            w / 2. - (x + rw / 2.) * self.zoom,
            self.top_inset + free / 2. - (y + rh / 2.) * self.zoom,
        ];
        true
    }

    /// Sets the page size. Once the view has fitted it stays where it is:
    /// a page that changes size from an edit (a template root sized by a
    /// value) must not yank the camera. `refit` fits the new page.
    pub fn resize(&mut self, size: [f32; 2]) {
        if self.region.is_none()
            && let Some(f) = &mut self.fitted
        {
            f[2] = size[0];
            f[3] = size[1];
        }
        self.size = size;
    }

    /// Frames `r` (design units) in a pane `w` × `h` with `margin` pixels
    /// around it, at a zoom within `max`. The view is the user's from then
    /// on: a pane that resizes keeps it. A box with no size centres where
    /// it is at the zoom there was.
    pub fn frame(&mut self, r: [f32; 4], w: f32, h: f32, margin: f32, max: f32) {
        self.auto = false;
        if r[2] > 0. && r[3] > 0. {
            self.zoom = ((w - margin) / r[2].max(8.))
                .min((h - margin) / r[3].max(8.))
                .clamp(*ZOOM.start(), max);
        }
        self.pan = [
            w / 2. - (r[0] + r[2] / 2.) * self.zoom,
            h / 2. - (r[1] + r[3] / 2.) * self.zoom,
        ];
    }

    /// Fits again on the next `fit`, e.g. after the page size changed.
    pub fn refit(&mut self) {
        self.fitted = None;
        self.auto = true;
    }

    /// Zooms by a scroll delta in pixels, keeping the point under `cursor`
    /// (pane-relative) fixed.
    pub fn zoom_at(&mut self, delta: f32, cursor: [f32; 2]) {
        self.auto = false;
        let old = self.zoom;
        self.zoom = (old * (delta * 0.0025).exp()).clamp(*ZOOM.start(), *ZOOM.end());
        let k = self.zoom / old;
        self.pan = [
            cursor[0] - (cursor[0] - self.pan[0]) * k,
            cursor[1] - (cursor[1] - self.pan[1]) * k,
        ];
    }

    pub fn begin_pan(&mut self, at: Point<Pixels>) {
        self.auto = false;
        self.drag = Some(Drag::Pan(at));
    }

    /// Starts moving or resizing `item`, whose rectangle is `rect` now.
    pub fn begin_edit(&mut self, item: K, handle: Handle, at: Point<Pixels>, rect: [f32; 4]) {
        self.drag = Some(Drag::Edit {
            item,
            handle,
            start: at,
            rect,
        });
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The edit in progress: the item, its handle and its rectangle when
    /// the drag began.
    pub fn editing(&self) -> Option<(K, Handle, [f32; 4])> {
        match self.drag {
            Some(Drag::Edit {
                item, handle, rect, ..
            }) => Some((item, handle, rect)),
            _ => None,
        }
    }

    /// The rectangle an edit drag shows for `item` with the pointer at `at`,
    /// its moved edges rounded to whole design units. `None` unless `item`
    /// is being edited.
    /// Resizing never goes below one unit.
    pub fn preview(&self, item: K, at: Point<Pixels>) -> Option<[f32; 4]> {
        let Some(Drag::Edit {
            item: dragged,
            handle,
            start,
            rect,
        }) = self.drag
        else {
            return None;
        };
        if dragged != item {
            return None;
        }
        let dx = f32::from(at.x - start.x) / self.zoom;
        let dy = f32::from(at.y - start.y) / self.zoom;
        let [mut x, mut y, mut w, mut h] = rect;
        // A move lands on whole units and keeps the size as it is: a
        // fractional size rounded would read as a resize.
        let Handle::Resize(k) = handle else {
            return Some([(x + dx).round(), (y + dy).round(), w, h]);
        };
        let (fx, fy) = SPOTS[k % 8];
        if fx == 0. {
            x += dx;
            w -= dx;
        } else if fx == 1. {
            w += dx;
        }
        if fy == 0. {
            y += dy;
            h -= dy;
        } else if fy == 1. {
            h += dy;
        }
        Some([x, y, w.max(1.), h.max(1.)].map(f32::round))
    }

    /// Follows the pointer. Returns whether to redraw.
    pub fn drag_to(&mut self, at: Point<Pixels>) -> bool {
        match &mut self.drag {
            Some(Drag::Pan(last)) => {
                self.pan[0] += f32::from(at.x - last.x);
                self.pan[1] += f32::from(at.y - last.y);
                *last = at;
                true
            }
            Some(Drag::Edit { .. }) => true,
            None => false,
        }
    }

    /// Ends the drag. An edit that changed its rectangle is returned.
    pub fn finish(&mut self, at: Point<Pixels>) -> Option<Edit<K>> {
        let Drag::Edit { item, rect, .. } = self.drag? else {
            self.drag = None;
            return None;
        };
        let after = self.preview(item, at);
        self.drag = None;
        let after = after.filter(|a| *a != rect)?;
        Some(Edit {
            item,
            before: rect,
            after,
        })
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// The page background, to put behind the items.
    pub fn page(&self) -> AnyElement {
        let top_left = self.to_screen(0., 0.);
        div()
            .absolute()
            .left(top_left.x)
            .top(top_left.y)
            .w(px(self.size[0] * self.zoom))
            .h(px(self.size[1] * self.zoom))
            .bg(theme::get().artboard)
            .border_1()
            .border_color(theme::get().border)
            .into_any_element()
    }

    /// `elements` (positioned in pane coordinates, like [`Self::page`])
    /// cut to the page, as the game cuts a screen to the display. Handles
    /// and other editing chrome stay outside so they work at the edges.
    pub fn clip_to_page(&self, elements: Vec<AnyElement>) -> AnyElement {
        let top_left = self.to_screen(0., 0.);
        div()
            .absolute()
            .left(top_left.x)
            .top(top_left.y)
            .w(px(self.size[0] * self.zoom))
            .h(px(self.size[1] * self.zoom))
            .overflow_hidden()
            // Undo the offset, so the children keep their pane positions.
            .child(
                div()
                    .absolute()
                    .left(-top_left.x)
                    .top(-top_left.y)
                    .children(elements),
            )
            .into_any_element()
    }

    /// The eight resize handles of `rect` (design units). `on_down` gets
    /// the handle and the pointer; propagation stops at the handle.
    pub fn handles(
        &self,
        rect: [f32; 4],
        on_down: impl Fn(Handle, Point<Pixels>, &mut Window, &mut App) + Clone + 'static,
    ) -> Vec<AnyElement> {
        let p = self.to_screen(rect[0], rect[1]);
        let (sw, sh) = (rect[2] * self.zoom, rect[3] * self.zoom);
        // On a small rectangle the handles would cover it: the middle ones
        // go, and the corners sit outside it so its body stays grabbable.
        let (narrow, flat) = (sw < 3. * HANDLE, sh < 3. * HANDLE);
        let out = |f: f32, small: bool| match f {
            0. if small => -HANDLE / 2.,
            1. if small => HANDLE / 2.,
            _ => 0.,
        };
        SPOTS
            .into_iter()
            .enumerate()
            .filter(|&(_, (fx, fy))| !(narrow && fx == 0.5 || flat && fy == 0.5))
            .map(|(k, (fx, fy))| {
                let on = on_down.clone();
                div()
                    .id(("design-handle", k))
                    .absolute()
                    .left(p.x + px(sw * fx - HANDLE / 2. + out(fx, narrow)))
                    .top(p.y + px(sh * fy - HANDLE / 2. + out(fy, flat)))
                    .size(px(HANDLE))
                    .bg(theme::get().highlight)
                    .cursor(match k {
                        0 | 4 => CursorStyle::ResizeUpLeftDownRight,
                        2 | 6 => CursorStyle::ResizeUpRightDownLeft,
                        1 | 5 => CursorStyle::ResizeUpDown,
                        _ => CursorStyle::ResizeLeftRight,
                    })
                    .on_mouse_down(MouseButton::Left, move |e, window, cx| {
                        cx.stop_propagation();
                        on(Handle::Resize(k), e.position, window, cx);
                    })
                    .into_any_element()
            })
            .collect()
    }
}

/// A line a dragged rectangle snapped to, in design units: `vertical`
/// lines sit at x = `at` and run from y `from` to `to`; horizontal ones
/// the other way round.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guide {
    pub vertical: bool,
    pub at: f32,
    pub from: f32,
    pub to: f32,
}

/// Snaps the edges and centre of `rect`, moved or resized by `handle`, to
/// the nearest edge or centre of `targets` within `reach` design units,
/// each axis on its own. Only the edges a handle moves snap. Returns the
/// snapped rectangle and a guide per target line it now touches.
pub fn snap(
    rect: [f32; 4],
    handle: Handle,
    targets: &[[f32; 4]],
    reach: f32,
) -> ([f32; 4], Vec<Guide>) {
    let [mut x, mut y, mut w, mut h] = rect;
    // Which of begin / centre / end move along each axis.
    let (mx, my) = match handle {
        Handle::Move => ([true; 3], [true; 3]),
        Handle::Resize(k) => {
            let (fx, fy) = SPOTS[k % 8];
            let pick = |f: f32| [f == 0., false, f == 1.];
            (pick(fx), pick(fy))
        }
    };
    let lines = |o: f32, s: f32| [o, o + s / 2., o + s];
    let best = |own: [f32; 3], moving: [bool; 3], axis: usize| {
        let mut best: Option<(f32, usize)> = None;
        for t in targets {
            for tl in lines(t[axis], t[axis + 2]) {
                for (k, &ol) in own.iter().enumerate() {
                    let d = tl - ol;
                    if moving[k] && d.abs() <= reach && best.is_none_or(|(b, _)| d.abs() < b.abs())
                    {
                        best = Some((d, k));
                    }
                }
            }
        }
        best
    };
    let shift = |o: &mut f32, s: &mut f32, d: f32, k: usize, handle: Handle| match handle {
        Handle::Move => *o += d,
        _ if k == 0 => {
            *o += d;
            *s -= d;
        }
        _ => *s += d,
    };
    if let Some((d, k)) = best(lines(x, w), mx, 0) {
        shift(&mut x, &mut w, d, k, handle);
    }
    if let Some((d, k)) = best(lines(y, h), my, 1) {
        shift(&mut y, &mut h, d, k, handle);
    }
    let out = [x, y, w.max(1.), h.max(1.)];
    let mut guides = Vec::new();
    for t in targets {
        for (axis, moving) in [(0, mx), (1, my)] {
            let own = lines(out[axis], out[axis + 2]);
            for tl in lines(t[axis], t[axis + 2]) {
                if own
                    .iter()
                    .zip(moving)
                    .any(|(&ol, m)| m && (ol - tl).abs() < 0.01)
                {
                    let o = 1 - axis;
                    guides.push(Guide {
                        vertical: axis == 0,
                        at: tl,
                        from: out[o].min(t[o]),
                        to: (out[o] + out[o + 2]).max(t[o] + t[o + 2]),
                    });
                }
            }
        }
    }
    (out, guides)
}

#[cfg(test)]
mod tests {
    use super::{Design, Edit, Handle, snap};

    #[test]
    fn snapping_pulls_moving_edges_to_targets_within_reach() {
        let parent = [0., 0., 400., 300.];
        // 3 units off the parent's centre line: moves onto it.
        let (r, guides) = snap([153., 50., 100., 40.], Handle::Move, &[parent], 4.);
        assert_eq!(r, [150., 50., 100., 40.]);
        assert!(guides.iter().any(|g| g.vertical && g.at == 200.));
        // Out of reach: unchanged, no guides.
        let (r, guides) = snap([160., 50., 100., 40.], Handle::Move, &[parent], 4.);
        assert_eq!(r, [160., 50., 100., 40.]);
        assert!(guides.is_empty());
        // A right-edge resize only moves the right edge.
        let (r, _) = snap([10., 10., 387., 40.], Handle::Resize(3), &[parent], 4.);
        assert_eq!(r, [10., 10., 390., 40.]);
        // The left handle keeps the right edge where it was.
        let (r, _) = snap([2., 10., 98., 40.], Handle::Resize(7), &[parent], 4.);
        assert_eq!(r, [0., 10., 100., 40.]);
    }
    use gpui::{point, px, size};

    #[test]
    fn move_and_resize_follow_the_pointer_in_design_units() {
        let mut d = Design::<usize> {
            zoom: 2.,
            ..Default::default()
        };
        let rect = [10., 10., 100., 50.];
        d.begin_edit(7, Handle::Move, point(px(0.), px(0.)), rect);
        assert_eq!(d.preview(3, point(px(9.), px(9.))), None);
        assert_eq!(
            d.finish(point(px(20.), px(-10.))),
            Some(Edit {
                item: 7,
                before: rect,
                after: [20., 5., 100., 50.]
            })
        );
        // The bottom-right corner grows the size only.
        d.begin_edit(7, Handle::Resize(4), point(px(0.), px(0.)), rect);
        assert_eq!(
            d.finish(point(px(10.), px(10.))).unwrap().after,
            [10., 10., 105., 55.]
        );
        // The top-left corner moves the origin and shrinks, never below 1 unit.
        d.begin_edit(7, Handle::Resize(0), point(px(0.), px(0.)), rect);
        assert_eq!(
            d.finish(point(px(400.), px(0.))).unwrap().after,
            [210., 10., 1., 50.]
        );
        // The left edge leaves the height alone.
        d.begin_edit(7, Handle::Resize(7), point(px(0.), px(0.)), rect);
        assert_eq!(
            d.finish(point(px(-20.), px(30.))).unwrap().after,
            [0., 10., 110., 50.]
        );
        // A move keeps a fractional size exactly.
        d.begin_edit(
            7,
            Handle::Move,
            point(px(0.), px(0.)),
            [0.5, 0.5, 10.25, 4.75],
        );
        assert_eq!(
            d.finish(point(px(4.), px(0.))).unwrap().after,
            [3., 1., 10.25, 4.75]
        );
        // A click without movement is not an edit, and a pan is never one.
        d.begin_edit(7, Handle::Move, point(px(3.), px(3.)), rect);
        assert!(d.finish(point(px(3.), px(3.))).is_none());
        d.begin_pan(point(px(0.), px(0.)));
        assert!(d.drag_to(point(px(5.), px(-5.))));
        assert!(d.finish(point(px(5.), px(-5.))).is_none() && !d.dragging());
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor_and_fit_centers_once() {
        let mut d = Design::<usize>::default();
        let before = d.to_design(point(px(300.), px(200.)));
        d.zoom_at(400., [300., 200.]);
        let after = d.to_design(point(px(300.), px(200.)));
        assert!((before[0] - after[0]).abs() < 1e-3 && (before[1] - after[1]).abs() < 1e-3);
        d.fit(size(px(1000.), px(600.)));
        assert!((d.zoom - 0.5).abs() < 1e-6 && (d.pan[0] - 20.).abs() < 1e-3);
        d.zoom = 3.;
        d.fit(size(px(1000.), px(600.)));
        assert_eq!(d.zoom, 3.);
        d.refit();
        d.fit(size(px(1000.), px(600.)));
        assert!((d.zoom - 0.5).abs() < 1e-6);
        // A region frames that part of the page instead.
        let mut r = Design::<usize>::default();
        r.region = Some([0., 800., 1920., 200.]);
        r.fit(size(px(1000.), px(600.)));
        let mid = r.to_screen(960., 900.);
        assert!((f32::from(mid.x) - 500.).abs() < 1e-3 && (f32::from(mid.y) - 300.).abs() < 1e-3);
        assert!(r.zoom > 0.45 && r.zoom < 0.5);
        // A document that loads after the first layout brings its own size.
        d.size = [3840., 2160.];
        d.fit(size(px(1000.), px(600.)));
        assert!((d.zoom - 0.25).abs() < 1e-6);
    }
}
