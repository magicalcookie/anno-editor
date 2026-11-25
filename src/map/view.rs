// The Map workspace's centre pane: the toolbar, the variant tabs, and one
// canvas that draws the sea, the slots and the start points with a draw call
// each (never a div per slot), plus the tools' presses, drags and keys.

use super::{Drag, MapEditor, Tool, panels};
use crate::editor::{Editor, Pane};
use anno_core::game::{Module, SizeClass};
use anno_core::map::{MapRules, Role};
use anno_formats::vfs::Install;
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    paint, theme,
    widgets::{self, button, hint},
};
use std::sync::Arc;

/// Pixels a side of an island's top view: a Large 117 island fills a
/// third of a 1080p canvas at most, and four turns of tens of islands stay
/// a few megabytes.
const TOP_PX: u32 = 384;

/// Fill colour per role, RGB; a slot's outline and label share it.
fn role_color(role: Role) -> u32 {
    match role {
        Role::Normal => 0x6fb36f,
        Role::Start => 0xffc857,
        Role::ThirdParty => 0x9a7fe0,
        Role::Pirate => 0xe05a5a,
        Role::Volcano => 0xe08a3c,
        Role::Decoration => 0x8a949e,
        Role::Underwater => 0x3fa9ff,
        Role::ThirdPartyUnderwater => 0x5f6fe0,
        Role::Marker => 0xd0d0d0,
        Role::Other => 0xb0b0b0,
    }
}

/// The class's short name on a pool slot.
fn class_label(classes: &[SizeClass], class: u8) -> &'static str {
    match classes.get(class as usize).map(|c| c.name) {
        Some("Small") => "S",
        Some("Medium") => "M",
        Some("Large") => "L",
        Some("ExtraLarge") | Some("XL") | Some("Extra Large") => "XL",
        Some(name) => name,
        None => "XL",
    }
}

/// An island's top view at `TOP_PX`, turned 0 to 3 quarter turns
/// clockwise, sea transparent. 117 and 1800 take the island's minimap
/// picture uncut, so it lies at true size over the slot; other games their
/// codec's `top_view`.
pub fn top_views(
    module: &dyn Module,
    install: &Install,
    path: &str,
) -> [Option<Arc<RenderImage>>; 4] {
    let image = match module.islands().map(|i| i.codec()) {
        Some(anno_core::game::Codec::A7m(_)) => {
            crate::island::catalog::land_image(install, path, TOP_PX).map(|img| {
                image::imageops::resize(&img, TOP_PX, TOP_PX, image::imageops::FilterType::Triangle)
            })
        }
        Some(anno_core::game::Codec::Own(codec)) => codec
            .top_view(install, path, TOP_PX)
            .and_then(|t| image::RgbaImage::from_raw(t.width, t.height, t.rgba)),
        None => None,
    };
    let Some(image) = image else {
        return Default::default();
    };
    let turned = [
        image.clone(),
        image::imageops::rotate90(&image),
        image::imageops::rotate180(&image),
        image::imageops::rotate270(&image),
    ];
    turned.map(|img| {
        let (w, h) = img.dimensions();
        gpui_editor_kit::texture::Texture::from_rgba(w, h, img.into_raw())
            .ok()
            .map(|t| t.render_image())
    })
}

/// One slot as the canvas draws it, in metres.
struct Drawn {
    rect: [f32; 4],
    color: u32,
    top: Option<Arc<RenderImage>>,
    label: &'static str,
    start: Option<usize>,
    selected: bool,
    broken: bool,
}

impl MapEditor {
    /// Map point under window point `p`, from the canvas rectangle the last
    /// paint measured.
    pub fn to_map(&self, p: Point<Pixels>) -> [f32; 2] {
        let c = &self.camera;
        let [x, y, w, h] = self.rect;
        [
            (f32::from(p.x) - x - w / 2.) / c.scale + c.centre[0],
            (f32::from(p.y) - y - h / 2.) / c.scale + c.centre[1],
        ]
    }

    /// The camera that shows the whole map in a canvas `w` by `h`.
    fn fit(&mut self, w: f32, h: f32) {
        let [mw, mh] = self.size().map(|v| v.max(1) as f32);
        self.camera.scale = (w / mw).min(h / mh) * 0.92;
        self.camera.centre = [mw / 2., mh / 2.];
        self.camera.fitted = true;
    }

    /// Zooms by `factor` about window point `p`, keeping the map point
    /// under it there.
    pub fn zoom_at(&mut self, p: Point<Pixels>, factor: f32) {
        let before = self.to_map(p);
        self.camera.scale = (self.camera.scale * factor).clamp(0.02, 8.);
        let after = self.to_map(p);
        for k in 0..2 {
            self.camera.centre[k] += before[k] - after[k];
        }
    }
}

impl Editor {
    pub fn render_map_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(maps) = self.maps() else {
            return crate::editor::empty_pane("This game's maps don't open in the editor");
        };
        self.map_facts_need_list(cx);
        // The dialogs list shipped maps once assets.xml is in.
        if self.map_dialog.is_some() {
            self.load_shipped_maps(cx);
        }
        let Some(m) = &self.map else {
            return self.render_map_empty(cx);
        };
        let rules = maps.rules();
        let classes = rules.size_classes;
        let document = self.map_document_bar(cx);
        let bar = self.map_toolbar(rules, window, cx);
        // Everything the paint needs, owned, in metres: the paint runs
        // after this borrow ends.
        let slots: Vec<Drawn> = (m.slots().iter().enumerate())
            .map(|(i, s)| {
                let [x0, y0, x1, y1] = m.footprint(s, rules).map(|v| v as f32);
                let top = (s.fixed())
                    .then(|| m.tops.get(s.island as usize))
                    .flatten()
                    .and_then(|t| t.turns[s.turns as usize % 4].clone());
                Drawn {
                    rect: [x0, y0, x1, y1],
                    color: role_color(s.role),
                    top,
                    label: if s.fixed() {
                        ""
                    } else {
                        class_label(classes, s.class)
                    },
                    start: None,
                    selected: m.selected.has(i),
                    broken: m.problems_of(i).next().is_some(),
                }
            })
            .collect();
        // Start slots are numbered in slot order, as players are.
        let mut slots = slots;
        let mut player = 0;
        for (d, s) in slots.iter_mut().zip(m.slots()) {
            if s.role == Role::Start {
                player += 1;
                d.start = Some(player);
            }
        }
        let starts: Vec<([f32; 2], bool)> = (m.starts().iter().enumerate())
            .map(|(n, p)| ([p[0] as f32, p[1] as f32], m.start == Some(n)))
            .collect();
        let size = m.size().map(|v| v as f32);
        let playable = m.playable().map(|v| v as f32);
        let grid = (rules.grid * 4).max(16) as f32;
        let camera = m.camera;
        let entity = cx.entity();
        let sea = rgb(crate::colors::SEA);
        let canvas = canvas(
            move |bounds, _, cx| {
                // The rectangle presses map through, and the first fit.
                let rect = [
                    f32::from(bounds.origin.x),
                    f32::from(bounds.origin.y),
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                ];
                entity.update(cx, |this, _| {
                    if let Some(m) = &mut this.map {
                        m.rect = rect;
                        if !m.camera.fitted && rect[2] > 0. && rect[3] > 0. {
                            m.fit(rect[2], rect[3]);
                        }
                    }
                });
                rect
            },
            move |bounds, rect, window, cx| {
                let mut c = camera;
                if !c.fitted {
                    let [mw, mh] = size.map(|v| v.max(1.));
                    c.scale = (rect[2] / mw).min(rect[3] / mh) * 0.92;
                    c.centre = [mw / 2., mh / 2.];
                }
                let to = |x: f32, y: f32| {
                    point(
                        px((x - c.centre[0]) * c.scale + rect[0] + rect[2] / 2.),
                        px((y - c.centre[1]) * c.scale + rect[1] + rect[3] / 2.),
                    )
                };
                let bounds_of = |r: [f32; 4]| {
                    let a = to(r[0], r[1]);
                    let b = to(r[2], r[3]);
                    Bounds::from_corners(a, b)
                };
                window.paint_quad(fill(bounds, theme::get().viewport));
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    window.paint_quad(fill(bounds_of([0., 0., size[0], size[1]]), sea));
                    // A line every four grid steps: 117's 32 m grid draws
                    // one every 128 m, a ship's turning circle or so.
                    let line = rgba(0xffffff14);
                    let mut x = grid;
                    while x < size[0] {
                        paint::line(&[to(x, 0.), to(x, size[1])], 1., line, window);
                        x += grid;
                    }
                    let mut y = grid;
                    while y < size[1] {
                        paint::line(&[to(0., y), to(size[0], y)], 1., line, window);
                        y += grid;
                    }
                    window.paint_quad(outline(
                        bounds_of([0., 0., size[0], size[1]]),
                        theme::get().border,
                        BorderStyle::Solid,
                    ));
                    window.paint_quad(outline(
                        bounds_of(playable),
                        rgba(0xffffff60),
                        BorderStyle::Dashed,
                    ));
                    let font = window.text_style().font();
                    for d in &slots {
                        let color = rgb(d.color);
                        // A marker is a point (no footprint): a dot.
                        if d.rect[0] == d.rect[2] {
                            let at = to(d.rect[0], d.rect[1]);
                            paint::filled_circle(at, 5., color, window);
                            if d.selected {
                                paint::circle(at, 8., 2., rgb(crate::colors::SELECTION), window);
                            }
                            continue;
                        }
                        let b = bounds_of(d.rect);
                        match &d.top {
                            Some(img) => {
                                let _ = window.paint_image(
                                    b,
                                    b,
                                    Corners::default(),
                                    img.clone(),
                                    0,
                                    false,
                                );
                                window.paint_quad(outline(
                                    b,
                                    rgba(d.color << 8 | 0x90),
                                    BorderStyle::Solid,
                                ));
                            }
                            None => {
                                window.paint_quad(quad(
                                    b,
                                    px(2.),
                                    rgba(d.color << 8 | 0x40),
                                    px(1.),
                                    color,
                                    if d.label.is_empty() {
                                        BorderStyle::Solid
                                    } else {
                                        BorderStyle::Dashed
                                    },
                                ));
                            }
                        }
                        if d.start.is_some() {
                            window.paint_quad(quad(
                                b,
                                px(2.),
                                transparent_black(),
                                px(3.),
                                theme::get().accent,
                                BorderStyle::Solid,
                            ));
                        }
                        if d.broken {
                            window.paint_quad(quad(
                                b.dilate(px(3.)),
                                px(3.),
                                transparent_black(),
                                px(2.),
                                theme::get().error,
                                BorderStyle::Solid,
                            ));
                        }
                        if d.selected {
                            window.paint_quad(quad(
                                b.dilate(px(5.)),
                                px(4.),
                                transparent_black(),
                                px(2.),
                                rgb(crate::colors::SELECTION),
                                BorderStyle::Solid,
                            ));
                        }
                        let text = match (d.start, d.label) {
                            (Some(n), "") => format!("P{n}"),
                            (Some(n), l) => format!("{l} · P{n}"),
                            (None, l) => l.to_owned(),
                        };
                        if !text.is_empty() && f32::from(b.size.width) > 14. {
                            let size = px(12.);
                            let line = window.text_system().shape_line(
                                text.clone().into(),
                                size,
                                &[TextRun {
                                    len: text.len(),
                                    font: font.clone(),
                                    color: rgb(0xffffff).into(),
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                }],
                                None,
                            );
                            let at = b.center() - point(line.width / 2., px(8.));
                            let _ = line.paint(at, px(16.), TextAlign::Left, None, window, cx);
                        }
                    }
                    // 2070's arks: pins at their point.
                    for (p, selected) in &starts {
                        let at = to(p[0], p[1]);
                        paint::filled_circle(at, 7., theme::get().accent, window);
                        paint::circle(
                            at,
                            7.,
                            2.,
                            if *selected {
                                rgb(crate::colors::SELECTION)
                            } else {
                                rgb(0xffffff)
                            },
                            window,
                        );
                    }
                });
            },
        )
        .size_full();
        let surface = div()
            .id("map-canvas")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(canvas)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::map_press))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::map_pan_press))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::map_pan_press))
            .on_mouse_move(cx.listener(Self::map_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::map_release))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::map_release))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::map_release))
            .on_scroll(cx.listener(|this, e: &ScrollEvent, window, cx| {
                let dy = f32::from(e.delta.pixel_delta(window.line_height()).y);
                if let Some(m) = &mut this.map {
                    m.zoom_at(e.position, (dy / 400.).exp());
                    this.notify_pane(Pane::MapView, cx);
                }
            }))
            // An island dragged from the Islands tab: onto a slot, or as a
            // new one where it lands.
            .drag_over::<panels::IslandDrag>(|d, _, _, _| {
                d.border_2().border_color(theme::get().accent)
            })
            .on_drop(cx.listener(|this, drag: &panels::IslandDrag, window, cx| {
                let at = window.mouse_position();
                this.drop_island_at(at, &drag.0, cx);
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .children(document)
            .child(bar)
            .child(surface)
            .child(self.map_key_hints(rules))
            .into_any_element()
    }

    /// The open layout's name, what it is, whether it has unsaved edits,
    /// and New, Open and Close: the same actions as the Map menu and the
    /// empty state.
    fn map_document_bar(&self, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.map.as_ref()?;
        let kind = match (m.shipped, &m.path) {
            (true, _) => "Shipped map: saving asks how to keep it",
            (false, Some(_)) => "Project layout",
            (false, None) => "New layout, not saved yet",
        };
        let dirty = m.dirty();
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme::get().border)
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(m.title()),
                )
                .when(dirty, |d| {
                    d.child(
                        div()
                            .id("map-unsaved")
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(theme::get().highlight)
                            .child("● Unsaved")
                            .tooltip(widgets::Tooltip::text(
                                "Unsaved edits",
                                "Ctrl S",
                                "Ctrl S saves them into the project",
                            )),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(theme::get().muted)
                        .child(kind),
                )
                .child(
                    button("New…", false).on_click(cx.listener(|this, _, _, cx| {
                        this.open_new_map(cx);
                        cx.notify();
                    })),
                )
                .child(
                    button("Open…", false).on_click(cx.listener(|this, _, _, cx| {
                        this.open_map_dialog(cx);
                        cx.notify();
                    })),
                )
                .child(
                    button("Close", false).on_click(cx.listener(|this, _, _, cx| {
                        this.leave(crate::editor::Leave::CloseMap, cx);
                        cx.notify();
                    })),
                ),
        )
    }

    /// No map open: what to start from.
    fn render_map_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let layouts: Vec<(String, String)> = (self.project.layouts.iter())
            .map(|l| (l.name.clone(), l.path.clone()))
            .collect();
        widgets::empty_state("No map layout open")
            .child(
                "Start a layout on a shipped sea, or open one of this project's layouts or the game's maps.",
            )
            .child(
                div()
                    .pt_2()
                    .flex()
                    .gap_2()
                    .child(
                        widgets::action("New map layout…", widgets::Tone::Primary).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.open_new_map(cx);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        widgets::action("Open map…", widgets::Tone::Secondary).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.open_map_dialog(cx);
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .when(!layouts.is_empty(), |d| {
                d.child(div().pt_3().child(hint("This project's layouts")))
                    .children(layouts.into_iter().map(|(name, path)| {
                        button(name, false).on_click(cx.listener(move |this, _, _, cx| {
                            this.open_map(&path, cx);
                            cx.notify();
                        }))
                    }))
            })
            .into_any_element()
    }

    /// Tools, the role and class Add places, the variant tabs and the
    /// variant actions.
    fn map_toolbar(&self, rules: &'static MapRules, _: &mut Window, cx: &mut Context<Self>) -> Div {
        let classes = rules.size_classes;
        let Some(m) = &self.map else {
            return div();
        };
        let mut tools = vec![(Tool::Select, "Select  V"), (Tool::Add, "Add slot  A")];
        if rules.free_starts {
            tools.push((Tool::Starts, "Start points  S"));
        }
        let tool = widgets::segmented(
            "map-tool",
            tools.into_iter().map(|(t, l)| (t, l.into())),
            &m.tool,
            cx,
            |this, t, _, _| {
                if let Some(m) = &mut this.map {
                    m.tool = t;
                }
            },
        );
        let roles = widgets::segmented(
            "map-role",
            (rules.roles.iter().enumerate())
                .map(|(n, (r, _))| (*r, format!("{} {}", n + 1, r.label()).into())),
            &m.add_role,
            cx,
            |this, role, _, cx| this.map_set_role(role, cx),
        );
        let class = widgets::segmented(
            "map-class",
            (0..classes.len() as u8).map(|c| (c, class_label(classes, c).into())),
            &m.add_class,
            cx,
            |this, c, _, cx| {
                if let Some(m) = &mut this.map {
                    m.set_class(c);
                }
                this.map_edited(cx);
            },
        );
        // Tabs as `segmented` draws them, a variant with a problem in the
        // error colour: 1800's nine are too many to check one by one.
        let variants = (m.map.variants.len() > 1).then(|| {
            let mut row = div()
                .flex()
                .flex_shrink_0()
                .gap_px()
                .p_px()
                .rounded(px(3.))
                .bg(theme::get().border);
            for (n, v) in m.map.variants.iter().enumerate() {
                let current = n == m.variant;
                let broken = m.variant_has_problems(n);
                row = row.child(
                    button(variant_label(&m.map.variants, v), current)
                        .h(px(24.))
                        .text_size(px(11.))
                        .when(!current, |b| b.bg(theme::get().input))
                        .when(broken, |b| b.text_color(theme::get().error))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(m) = &mut this.map {
                                m.show_variant(n);
                            }
                            cx.notify();
                        })),
                );
            }
            div().id("map-variant").flex().child(row)
        });
        let multi = m.map.variants.len() > 1;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(theme::get().border)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(tool)
                    .child(widgets::toolbar_separator())
                    .child(roles)
                    .when(rules.pool_slots, |d| d.child(class)),
            )
            .when(multi, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .children(variants)
                        .child(
                            button(copy_label(m.map.variants.len()), false)
                                .tooltip(widgets::Tooltip::text(
                                    copy_label(m.map.variants.len()),
                                    "",
                                    copy_what(m),
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.copy_variant(cx))),
                        )
                        .child(
                            widgets::checkbox(
                                "Edit all variants",
                                m.all_variants,
                                "moves and edits follow in the others",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(m) = &mut this.map {
                                    m.all_variants = !m.all_variants;
                                }
                                cx.notify();
                            })),
                        ),
                )
            })
    }

    /// Copies the shown variant's slots and starts over the others, saying
    /// what it replaced.
    pub(crate) fn copy_variant(&mut self, cx: &mut Context<Self>) {
        let Some(m) = &mut self.map else {
            return;
        };
        let done = copy_what(m);
        m.copy_to_other_variants();
        self.map_edited(cx);
        self.message(format!("{done}. Ctrl Z undoes it"), false);
    }

    /// The canvas's keys, only those this game's maps have.
    fn map_key_hints(&self, rules: &MapRules) -> Div {
        let mut keys = vec!["Drag: move", "Shift click: add to selection", "Del: delete"];
        if rules.fixed_turns {
            keys.push("R: turn an island");
        }
        let roles = match rules.roles.len() {
            0 => String::new(),
            1 => "1: role".to_owned(),
            n => format!("1–{}: role", n.min(9)),
        };
        if !roles.is_empty() {
            keys.push(&roles);
        }
        if rules.pool_slots {
            keys.push("[ ]: class");
        }
        keys.extend(["wheel: zoom", "right drag: pan"]);
        div()
            .flex()
            .flex_wrap()
            .gap_3()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme::get().border)
            .text_size(px(11.))
            .text_color(theme::get().muted)
            .child(keys.join(" · "))
    }

    fn map_press(&mut self, e: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(maps) = self.maps() else {
            return;
        };
        let rules = maps.rules();
        let Some(m) = &mut self.map else {
            return;
        };
        let at = m.to_map(e.position);
        let reach = 10. / m.camera.scale.max(0.01);
        match m.tool {
            Tool::Starts => match m.hit_start(at, reach) {
                Some(n) => m.begin_start_move(n, at),
                None => m.add_start(at, rules.grid),
            },
            Tool::Add if rules.pool_slots => {
                m.add_slot(at, rules);
            }
            Tool::Add => {
                m.message = Some(("Drag an island from the Islands tab onto the sea: every slot of this game names its island".into(), false));
            }
            Tool::Select => {
                if rules.free_starts
                    && let Some(n) = m.hit_start(at, reach)
                {
                    m.begin_start_move(n, at);
                } else {
                    let hit = m.hit(at, rules);
                    m.select(
                        hit,
                        e.modifiers.shift || e.modifiers.control || e.modifiers.platform,
                    );
                    if hit.is_some() && !m.selected.is_empty() {
                        m.begin_move(at);
                    }
                }
            }
        }
        self.focus_map();
        self.map_edited(cx);
        cx.notify();
    }

    fn map_pan_press(&mut self, e: &MouseDownEvent, _: &mut Window, _: &mut Context<Self>) {
        if let Some(m) = &mut self.map {
            m.drag = Some(Drag::Pan {
                from: [f32::from(e.position.x), f32::from(e.position.y)],
                centre: m.camera.centre,
            });
        }
    }

    fn map_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(maps) = self.maps() else {
            return;
        };
        let Some(m) = &mut self.map else {
            return;
        };
        match m.drag.clone() {
            Some(Drag::Pan { from, centre }) => {
                let s = m.camera.scale.max(0.01);
                m.camera.centre = [
                    centre[0] - (f32::from(e.position.x) - from[0]) / s,
                    centre[1] - (f32::from(e.position.y) - from[1]) / s,
                ];
                self.notify_pane(Pane::MapView, cx);
            }
            Some(_) if e.pressed_button == Some(MouseButton::Left) => {
                let at = m.to_map(e.position);
                m.drag_to(at, maps.rules().grid);
                self.notify_pane(Pane::MapView, cx);
            }
            _ => {}
        }
    }

    fn map_release(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let moved = match self.map.as_mut().and_then(|m| m.drag.take()) {
            Some(Drag::Slots { moved, .. }) => moved,
            Some(Drag::Start { .. }) => true,
            _ => false,
        };
        if moved {
            self.map_edited(cx);
            cx.notify();
        }
    }

    pub(crate) fn drop_island_at(&mut self, at: Point<Pixels>, path: &str, cx: &mut Context<Self>) {
        let Some(maps) = self.maps() else {
            return;
        };
        let facts = super::island_facts(self, path);
        if let Some(m) = &mut self.map {
            let p = m.to_map(at);
            m.drop_island(p, path, facts, maps.rules());
        }
        self.map_edited(cx);
        cx.notify();
    }

    pub(crate) fn map_set_role(&mut self, role: Role, cx: &mut Context<Self>) {
        let Some(maps) = self.maps() else {
            return;
        };
        if let Some(m) = &mut self.map {
            m.set_role(role, maps.rules());
        }
        self.map_edited(cx);
    }

    /// Keys while the Map workspace has them; false for keys it leaves to
    /// the window (Ctrl S, Ctrl Z, workspace switches).
    pub(crate) fn map_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(maps) = self.maps() else {
            return false;
        };
        let rules = maps.rules();
        let mods = event.keystroke.modifiers;
        if mods.control || mods.platform || mods.alt {
            return false;
        }
        let Some(m) = &mut self.map else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        match key {
            "v" => m.tool = Tool::Select,
            "a" => m.tool = Tool::Add,
            "s" if rules.free_starts => m.tool = Tool::Starts,
            "delete" | "backspace" => {
                m.delete_selected();
            }
            "r" => m.rotate_selected(rules),
            "[" => m.step_class(-1, rules),
            "]" => m.step_class(1, rules),
            "escape" => {
                m.selected.clear();
                m.start = None;
            }
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_digit() => {
                let n = (k.as_bytes()[0] - b'0') as usize;
                match n.checked_sub(1).and_then(|i| rules.roles.get(i)) {
                    Some(&(role, _)) => m.set_role(role, rules),
                    None => return false,
                }
            }
            _ => return false,
        }
        self.map_edited(cx);
        true
    }

    fn focus_map(&mut self) {
        let i = self.dock_index();
        self.docks[i].focus(Pane::MapView);
    }
}

/// "Copy layout to the other N variants".
pub fn copy_label(variants: usize) -> String {
    match variants.saturating_sub(1) {
        1 => "Copy layout to the other variant".to_owned(),
        n => format!("Copy layout to the other {n} variants"),
    }
}

/// What the copy does to the open map: which variant's layout replaces
/// what in how many others.
fn copy_what(m: &super::MapEditor) -> String {
    let n = m.map.variants.len().saturating_sub(1);
    let from = (m.map.variants.get(m.variant))
        .map_or_else(String::new, |v| variant_label(&m.map.variants, v));
    format!(
        "Replaces the slots and starts of {} with {from}'s",
        crate::project::plural(n, "other variant", "other variants")
    )
}

/// A variant tab: the setup option it answers. The world size only where
/// the variants differ in it: every 117 template is world size Large.
pub fn variant_label(all: &[anno_core::map::Variant], v: &anno_core::map::Variant) -> String {
    let worlds = all.iter().any(|o| o.setup.world_size != v.setup.world_size);
    setup_label(v.setup, worlds)
}

/// A setup option in words, its world size only with `worlds`.
pub fn setup_label(setup: anno_core::map::Setup, worlds: bool) -> String {
    let world = if worlds { setup.world_size } else { "" };
    match (setup.island_size, world) {
        ("", "") => "Layout".to_owned(),
        (i, "") => format!("{i} islands"),
        ("", w) => w.to_owned(),
        (i, w) => format!("{w} · {i} islands"),
    }
}
