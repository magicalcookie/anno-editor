// Cache replay rules for cached views nested in other cached views.
use std::{cell::Cell, rc::Rc};

use gpui::{Context, Entity, StyleRefinement, TestAppContext, Window, div, prelude::*, px, size};

struct Inner {
    renders: Rc<Cell<usize>>,
}

impl Render for Inner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        div().size_full().child("inner text")
    }
}

struct Outer {
    inner: Entity<Inner>,
}

impl Render for Outer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child("outer text").child(
            self.inner
                .clone()
                .cached(StyleRefinement::default().size(px(50.))),
        )
    }
}

/// Text lines drawn ahead of `outer` in paint order, so they shift the
/// line-layout indices `outer` and `inner` record. Absolutely placed so
/// `outer`'s bounds, and with them its cache key, stay put.
struct Root {
    outer: Entity<Outer>,
    lines: usize,
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .absolute()
                    .left(px(200.))
                    .children((0..self.lines).map(|ix| div().child(format!("line {ix}")))),
            )
            .child(
                div().absolute().child(
                    self.outer
                        .clone()
                        .cached(StyleRefinement::default().size(px(100.))),
                ),
            )
    }
}

// A cached view replayed by its parent keeps the ranges it recorded the
// frame before. When the parent renders again the next frame, those
// ranges must not be replayed: they index buffers two frames old.
// This is the zoom crash in the island editor (`reuse_layouts` slicing
// past the end of the previous frame's line list).
#[gpui::test]
fn test_nested_cached_view_after_parent_replay(cx: &mut TestAppContext) {
    let inner_renders = Rc::new(Cell::new(0));
    let inner = cx.new(|_| Inner {
        renders: inner_renders.clone(),
    });
    let outer = cx.new(|_| Outer {
        inner: inner.clone(),
    });
    let window = cx.open_window(size(px(800.), px(600.)), |_, _| Root {
        outer: outer.clone(),
        lines: 8,
    });
    cx.run_until_parked();

    // Root renders with fewer lines; outer replays, inner's state keeps
    // the first frame's (higher) indices.
    window
        .update(cx, |root, _, cx| {
            root.lines = 0;
            cx.notify();
        })
        .unwrap();
    cx.run_until_parked();
    let renders = inner_renders.get();

    // Outer renders; inner must render too instead of replaying stale ranges.
    outer.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(inner_renders.get(), renders + 1);

    // Inner's ranges are fresh again, so the next replay reuses them.
    outer.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(inner_renders.get(), renders + 1);
}
