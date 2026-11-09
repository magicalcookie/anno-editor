// A small scene editor built only from the kit: docked 3D and top views,
// picking, gizmos, box selection, undo, an inspector and menus.
//
// cargo run --example demo
use gpui::{prelude::*, *};
use gpui_editor_kit::{
    camera::{Camera, Extent},
    dock::{self, Axis, Dock, DockHost, Layout},
    field::{EditEvent, Filter, NumberAction, TextEdit, number_field},
    gizmo::{self, Gesture, Marquee},
    gpu::{self, Batch, Instances, Light, Marker, Mesh, MeshData, Renderer, Scene, Shadows, Shape},
    history::History,
    icon::Icon,
    math::{self, Mat4},
    paint, platform, theme,
    widgets::{self, Tooltip, button, icon_button, info_row, menu_item, panel_header, section},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Pane {
    Perspective,
    Top,
    Notes,
}

#[derive(Clone)]
struct Object {
    id: u64,
    name: String,
    matrix: Mat4,
    color: u32,
}

#[derive(Clone, Default)]
struct Document {
    objects: Vec<Object>,
    next_id: u64,
}

impl Document {
    fn sample() -> Self {
        let mut doc = Self::default();
        for (i, color) in [0x5aa0e6, 0xe6905a, 0x7cc47a, 0xc9a2e8, 0xe8d36a]
            .into_iter()
            .enumerate()
        {
            let mut matrix = math::translate([i as f32 * 2.5 - 5., 0.5, (i % 2) as f32 * 3. - 1.5]);
            matrix[5] = 1. + i as f32 * 0.4;
            matrix[13] = matrix[5] * 0.5;
            doc.add(format!("Crate {}", i + 1), matrix, color);
        }
        doc
    }

    fn add(&mut self, name: String, matrix: Mat4, color: u32) -> u64 {
        self.next_id += 1;
        self.objects.push(Object {
            id: self.next_id,
            name,
            matrix,
            color,
        });
        self.next_id
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Object> {
        self.objects.iter_mut().find(|o| o.id == id)
    }
}

const EXTENT: Extent = Extent {
    center: [0.; 3],
    size: 24.,
};

enum Drag {
    Gizmo {
        gesture: Gesture,
        targets: Vec<(u64, Mat4)>,
        before: Document,
        changed: bool,
    },
    Marquee(Marquee),
    Orbit(Point<Pixels>),
    /// Last cursor, and the world point grabbed when the drag began.
    Pan(Point<Pixels>, math::Vec3),
}

struct Viewport {
    camera: Camera,
    bounds: Bounds<Pixels>,
    renderer: Arc<gpu::Viewport>,
    drag: Option<Drag>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Position(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    File,
    Edit,
    View,
}

struct Demo {
    doc: Document,
    history: History<Document>,
    selection: BTreeSet<u64>,
    dock: Dock<Pane>,
    views: [Viewport; 2],
    gizmo: gizmo::Mode,
    editing: Option<TextEdit<Field>>,
    menu: Option<Menu>,
    help: bool,
    status: String,
    error: bool,
    focus: FocusHandle,
    // Rebuilt when the document revision changes; shared by both viewports.
    scene_revision: Option<(u64, usize)>,
    ground: Arc<Mesh>,
    pond: Arc<Mesh>,
    cube: Arc<Mesh>,
}

impl Demo {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let default = Layout::split(
            Axis::Horizontal,
            0.62,
            Layout::tabs(vec![Pane::Perspective], Pane::Perspective),
            Layout::tabs(vec![Pane::Top, Pane::Notes], Pane::Top),
        );
        let path = platform::config_dir("gpui-editor-kit-demo").join("layout.json");
        let (dock, error) = Dock::persistent(path, default);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let renderer = Arc::new(Renderer::default());
        let perspective = Camera {
            zoom: 1.6,
            fov: 0.9,
            ..Camera::perspective()
        };
        Self {
            history: History::new(true),
            doc: Document::sample(),
            selection: BTreeSet::new(),
            dock,
            views: [
                Viewport {
                    camera: perspective,
                    bounds: Bounds::default(),
                    renderer: renderer.viewport(),
                    drag: None,
                },
                Viewport {
                    camera: Camera::default(),
                    bounds: Bounds::default(),
                    renderer: renderer.viewport(),
                    drag: None,
                },
            ],
            gizmo: gizmo::Mode::Move,
            editing: None,
            menu: None,
            help: false,
            status: error.map_or("Ready".into(), |e| format!("{e:#}")),
            error: false,
            focus,
            scene_revision: None,
            ground: Arc::new(ground()),
            pond: Arc::new(pond()),
            cube: Arc::new(gpu::cube(0xffffff, gpu::Material::Lit)),
        }
    }

    fn view_index(pane: Pane) -> Option<usize> {
        match pane {
            Pane::Perspective => Some(0),
            Pane::Top => Some(1),
            Pane::Notes => None,
        }
    }

    fn message(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.error = error;
    }

    fn refresh_scene(&mut self) {
        let key = (self.history.revision(), self.selection.len());
        if self.scene_revision == Some(key) {
            return;
        }
        self.scene_revision = Some(key);
        let mut batches = vec![Batch {
            mesh: self.ground.clone(),
            instances: Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![],
            }),
        }];
        // One batch per object keeps per-object color simple; a real editor
        // groups instances of the same model into one batch.
        for object in &self.doc.objects {
            let mut data = self.cube.data().clone();
            data.colors
                .iter_mut()
                .for_each(|c| *c = shade(*c, object.color));
            batches.push(Batch {
                mesh: Arc::new(Mesh::new(data).expect("tinted cube")),
                instances: Arc::new(Instances {
                    matrices: vec![object.matrix],
                    ids: vec![Some(object.id)],
                }),
            });
        }
        let markers = self
            .doc
            .objects
            .iter()
            .filter(|o| self.selection.contains(&o.id))
            .map(|o| Marker {
                position: math::translation(&o.matrix),
                radius: 7.,
                color: u32::from(theme::get().highlight) >> 8, // 0xRRGGBB
                shape: Shape::Ring,
            })
            .collect();
        batches.push(Batch {
            mesh: self.pond.clone(),
            instances: Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![],
            }),
        });
        let scene = Scene {
            batches,
            terrain: None,
            lines: Default::default(),
            markers: Arc::new(markers),
            light: Light {
                sun: [0.5, 0.8, 0.3],
                sun_color: [0.75; 3],
                sky: [0.5, 0.55, 0.62],
                ground: [0.32, 0.3, 0.28],
                shadows: Some(Shadows::default()),
                ..Default::default()
            },
            tone: None,
            sky: None,
            occlusion: None,
        };
        for view in &self.views {
            view.renderer.renderer().set_scene(&scene);
        }
    }

    fn pivot(&self) -> Option<math::Vec3> {
        let selected = self
            .doc
            .objects
            .iter()
            .filter(|o| self.selection.contains(&o.id));
        let (sum, n) = selected.fold(([0.; 3], 0), |(s, n), o| {
            (math::add(s, math::translation(&o.matrix)), n + 1)
        });
        (n > 0).then(|| math::scale(sum, 1. / n as f32))
    }

    fn gizmo_frame(&self, view: usize) -> Option<gizmo::Frame> {
        let v = &self.views[view];
        gizmo::Frame::new(self.gizmo, self.pivot()?, None, v.camera, v.bounds, EXTENT)
    }

    fn undo(&mut self, redo: bool) {
        let done = if redo {
            self.history.redo(&mut self.doc)
        } else {
            self.history.undo(&mut self.doc)
        };
        self.selection
            .retain(|id| self.doc.objects.iter().any(|o| o.id == *id));
        self.message(if done { "" } else { "Nothing to undo" }, false);
    }

    fn add_object(&mut self) {
        self.history.checkpoint(&self.doc);
        let n = self.doc.objects.len();
        let id = self.doc.add(
            format!("Crate {}", n + 1),
            math::translate([0., 0.5, 0.]),
            0xd0d4da,
        );
        self.selection = BTreeSet::from([id]);
    }

    fn delete_selected(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.history.checkpoint(&self.doc);
        self.doc.objects.retain(|o| !self.selection.contains(&o.id));
        self.selection.clear();
    }

    fn frame_selection(&mut self) {
        let pivot = self.pivot().unwrap_or([0.; 3]);
        let pane = self.dock.focused;
        if let Some(i) = Self::view_index(pane) {
            self.views[i].camera.frame(
                pivot,
                if self.selection.is_empty() { 20. } else { 4. },
                EXTENT,
            );
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = &mut self.editing {
            match edit.key_down(event) {
                EditEvent::Commit => {
                    let edit = self.editing.take().expect("active edit");
                    let Field::Position(axis) = edit.field;
                    match edit.number() {
                        Some(v) => self.set_position(axis, v),
                        None => self.message("Enter a finite number", true),
                    }
                }
                EditEvent::Cancel => self.editing = None,
                EditEvent::Changed | EditEvent::Ignored => {}
            }
            cx.notify();
            return;
        }
        let mods = event.keystroke.modifiers;
        let ctrl = mods.control || mods.platform;
        match event.keystroke.key.as_str() {
            "z" if ctrl => self.undo(mods.shift),
            "y" if ctrl => self.undo(true),
            "w" => self.gizmo = gizmo::Mode::Move,
            "e" => self.gizmo = gizmo::Mode::Rotate,
            "r" => self.gizmo = gizmo::Mode::Scale,
            "f" => self.frame_selection(),
            "n" if ctrl => self.add_object(),
            "delete" | "backspace" => self.delete_selected(),
            "escape" => {
                self.menu = None;
                self.help = false;
                for view in &mut self.views {
                    if let Some(Drag::Gizmo {
                        before,
                        changed: true,
                        ..
                    }) = view.drag.take()
                    {
                        self.doc = before;
                    }
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn set_position(&mut self, axis: usize, value: f32) {
        let Some(&id) = self.selection.iter().next_back() else {
            return;
        };
        self.history.checkpoint(&self.doc);
        if let Some(o) = self.doc.get_mut(id) {
            o.matrix[12 + axis] = value;
        }
    }

    fn mouse_down(&mut self, view: usize, event: &MouseDownEvent, cx: &mut Context<Self>) {
        self.menu = None;
        let gesture = (event.button == MouseButton::Left)
            .then(|| self.gizmo_frame(view))
            .flatten()
            .and_then(|f| Gesture::begin(f, event.position));
        let v = &mut self.views[view];
        v.drag = match event.button {
            MouseButton::Right if v.camera.tilt > 0. && !event.modifiers.shift => {
                let hit = v.renderer.pick(event.position).map(|h| h.position);
                v.camera.begin_orbit(hit, v.bounds, EXTENT);
                Some(Drag::Orbit(event.position))
            }
            MouseButton::Right | MouseButton::Middle => {
                // A perspective pan keeps this point under the cursor; nearer
                // and farther things slide at their own speed.
                let grab = v
                    .renderer
                    .pick(event.position)
                    .map_or_else(|| v.camera.view_center(v.bounds, EXTENT), |h| h.position);
                Some(Drag::Pan(event.position, grab))
            }
            MouseButton::Left => {
                if let Some(gesture) = gesture {
                    let targets = self
                        .doc
                        .objects
                        .iter()
                        .filter(|o| self.selection.contains(&o.id))
                        .map(|o| (o.id, o.matrix))
                        .collect();
                    Some(Drag::Gizmo {
                        gesture,
                        targets,
                        before: self.doc.clone(),
                        changed: false,
                    })
                } else {
                    Some(Drag::Marquee(Marquee::new(
                        event.position,
                        event.modifiers.shift,
                    )))
                }
            }
            _ => None,
        };
        cx.notify();
    }

    fn mouse_move(&mut self, view: usize, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if event.pressed_button.is_none() && self.views[view].drag.is_some() {
            self.mouse_up(view, cx);
        }
        let snap = event.modifiers.control || event.modifiers.platform;
        let v = &mut self.views[view];
        match &mut v.drag {
            Some(Drag::Orbit(last)) => {
                v.camera.orbit_by(event.position - *last);
                *last = event.position;
            }
            Some(Drag::Pan(last, grab)) => {
                v.camera.pan_at(event.position - *last, *grab, EXTENT);
                *last = event.position;
            }
            Some(Drag::Marquee(m)) => m.cursor = event.position,
            Some(Drag::Gizmo {
                gesture,
                targets,
                changed,
                ..
            }) => {
                if let Some(delta) = gesture.delta(event.position, snap.then_some(1.)) {
                    let pivot = gesture.frame.pivot();
                    for (id, start) in targets.iter() {
                        if let Some(o) = self.doc.objects.iter_mut().find(|o| o.id == *id) {
                            o.matrix = delta.apply(start, pivot);
                        }
                    }
                    *changed = true;
                    // The scene key follows history revisions, which only
                    // change on commit; force a rebuild while dragging.
                    self.scene_revision = None;
                }
            }
            None => {
                let hover = v.renderer.pick(event.position);
                self.status = hover.map_or(String::new(), |h| {
                    let [x, y, z] = h.position;
                    format!("X {x:.2}   Y {y:.2}   Z {z:.2}")
                });
                self.error = false;
            }
        }
        cx.notify();
    }

    fn mouse_up(&mut self, view: usize, cx: &mut Context<Self>) {
        match self.views[view].drag.take() {
            Some(Drag::Gizmo {
                before,
                changed: true,
                ..
            }) => {
                self.history.checkpoint_owned(before);
            }
            Some(Drag::Marquee(m)) => {
                let v = &self.views[view];
                let ids = if m.is_click() {
                    v.renderer
                        .pick(m.start)
                        .and_then(|h| h.id)
                        .into_iter()
                        .collect()
                } else {
                    m.select(
                        self.doc
                            .objects
                            .iter()
                            .map(|o| (o.id, math::translation(&o.matrix))),
                        v.camera,
                        v.bounds,
                        EXTENT,
                    )
                };
                if !m.additive {
                    self.selection.clear();
                }
                self.selection.extend(ids);
            }
            _ => {}
        }
        cx.notify();
    }

    fn viewport(&mut self, view: usize, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let paint_entity = entity.clone();
        div()
            .id(("viewport", view))
            .relative()
            .size_full()
            .bg(theme::get().viewport)
            .child(
                canvas(
                    move |bounds, _, cx| entity.update(cx, |this, _| this.views[view].bounds = bounds),
                    move |bounds, _, window, cx| {
                        let this = paint_entity.read(cx);
                        let v = &this.views[view];
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            v.renderer.paint(v.camera, EXTENT, bounds, window);
                            for o in this.doc.objects.iter().filter(|o| this.selection.contains(&o.id)) {
                                let corners = math::corners(this.cube.bounds())
                                    .map(|p| v.camera.project(math::transform(&o.matrix, p), bounds, EXTENT));
                                paint::box_outline(&corners, 1., theme::get().highlight.alpha(0.56), window);
                            }
                            let active = match &v.drag {
                                Some(Drag::Gizmo { gesture, .. }) => Some(gesture.handle),
                                _ => None,
                            };
                            if let Some(frame) = this.gizmo_frame(view) {
                                frame.paint(active, window);
                            }
                            if let Some(Drag::Marquee(m)) = &v.drag
                                && !m.is_click()
                            {
                                m.paint(window);
                            }
                        });
                    },
                )
                .size_full(),
            )
            .child(div().absolute().top_2().left_2().child(self.tools(cx)))
            .child(
                div()
                    .absolute()
                    .bottom_2()
                    .left_3()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("Left: select / gizmo · Right: orbit · Middle or Shift-right: pan · Scroll: zoom · W E R · F"),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, e, _, cx| this.mouse_down(view, e, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, e, _, cx| this.mouse_down(view, e, cx)))
            .on_mouse_down(MouseButton::Middle, cx.listener(move |this, e, _, cx| this.mouse_down(view, e, cx)))
            .on_mouse_move(cx.listener(move |this, e, _, cx| this.mouse_move(view, e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, _, _, cx| this.mouse_up(view, cx)))
            .on_mouse_up(MouseButton::Right, cx.listener(move |this, _, _, cx| this.mouse_up(view, cx)))
            .on_mouse_up(MouseButton::Middle, cx.listener(move |this, _, _, cx| this.mouse_up(view, cx)))
            .on_scroll(cx.listener(move |this, event: &ScrollEvent, _, cx| {
                let delta = match event.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                    ScrollDelta::Lines(p) => p.y * 32.,
                };
                let v = &mut this.views[view];
                let target = v.renderer.pick(event.position).map(|h| h.position);
                v.camera.zoom_toward(delta, event.position, target, v.bounds, EXTENT);
                cx.notify();
            }))
            .into_any_element()
    }

    fn tools(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut bar = widgets::floating_surface("viewport-tools").p_1().gap_1();
        for (mode, icon, label, key, detail) in [
            (
                gizmo::Mode::Move,
                Icon::MOVE,
                "Move",
                "W",
                "Drag an axis, a plane square or the center",
            ),
            (
                gizmo::Mode::Rotate,
                Icon::ROTATE,
                "Rotate",
                "E",
                "Drag a ring. Ctrl snaps to 15°",
            ),
            (
                gizmo::Mode::Scale,
                Icon::SCALE,
                "Scale",
                "R",
                "Drag an axis or the center for uniform scale",
            ),
        ] {
            bar = bar.child(
                icon_button(label, icon, self.gizmo == mode)
                    .tooltip(Tooltip::text(label, key, detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.gizmo = mode;
                        cx.notify();
                    })),
            );
        }
        bar.child(
            icon_button("Add", Icon::ADD, false)
                .tooltip(Tooltip::text(
                    "Add crate",
                    "Ctrl N",
                    "Adds a crate at the origin",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.add_object();
                    cx.notify();
                })),
        )
    }

    fn outliner(&self, cx: &mut Context<Self>) -> Div {
        let mut list = div().flex().flex_col().p_1();
        for o in &self.doc.objects {
            let id = o.id;
            list = list.child(
                button(o.name.clone(), self.selection.contains(&id))
                    .justify_start()
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if !event.modifiers().shift {
                            this.selection.clear();
                        }
                        this.selection.insert(id);
                        cx.notify();
                    })),
            );
        }
        widgets::side_panel(200., true)
            .child(panel_header("Outliner"))
            .child(list)
    }

    fn inspector(&self, cx: &mut Context<Self>) -> Div {
        let mut body = div().flex().flex_col().p_2();
        let selected = self
            .selection
            .iter()
            .next_back()
            .and_then(|id| self.doc.objects.iter().find(|o| o.id == *id));
        match selected {
            None => {
                body = body.child(
                    div()
                        .text_color(theme::get().muted)
                        .child("Nothing selected"),
                )
            }
            Some(o) => {
                body = body
                    .child(info_row("Name", o.name.clone()))
                    .child(info_row("Selected", self.selection.len().to_string()))
                    .child(section("Position"));
                for (axis, label) in ["X", "Y", "Z"].into_iter().enumerate() {
                    let editing = self
                        .editing
                        .as_ref()
                        .filter(|e| e.field == Field::Position(axis))
                        .map(|e| e.display());
                    body = body.child(number_field(
                        label,
                        o.matrix[12 + axis],
                        0.5,
                        editing,
                        cx,
                        move |this, action, window, cx| {
                            match action {
                                NumberAction::Step(delta) => {
                                    let value = this
                                        .doc
                                        .objects
                                        .iter()
                                        .find(|o| this.selection.contains(&o.id))
                                        .map(|o| o.matrix[12 + axis])
                                        .unwrap_or(0.);
                                    this.set_position(axis, value + delta);
                                }
                                NumberAction::Edit => {
                                    this.focus.focus(window, cx);
                                    let value = this.selection.iter().next_back().and_then(|id| {
                                        this.doc
                                            .objects
                                            .iter()
                                            .find(|o| o.id == *id)
                                            .map(|o| o.matrix[12 + axis])
                                    });
                                    this.editing = Some(TextEdit::new(
                                        Field::Position(axis),
                                        format!("{:.2}", value.unwrap_or_default()),
                                        Filter::Number,
                                    ));
                                }
                            }
                            cx.notify();
                        },
                    ));
                }
            }
        }
        widgets::side_panel(260., false)
            .child(panel_header("Inspector"))
            .child(body)
    }

    fn menu_popup(&self, menu: Menu, cx: &mut Context<Self>) -> Stateful<Div> {
        let close = cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        let item = |label: &'static str, shortcut: &'static str, f: fn(&mut Self)| {
            menu_item(label, shortcut).on_click(cx.listener(move |this, _, _, cx| {
                this.menu = None;
                f(this);
                cx.notify();
            }))
        };
        let popup = widgets::menu_popup(match menu {
            Menu::File => 4.,
            Menu::Edit => 40.,
            Menu::View => 78.,
        })
        .on_mouse_down_out(close);
        match menu {
            Menu::File => popup
                .child(item("Add crate", "Ctrl N", Self::add_object))
                .child(item("Mark saved", "", |this| this.history.mark_saved())),
            Menu::Edit => popup
                .child(item("Undo", "Ctrl Z", |this| this.undo(false)))
                .child(item("Redo", "Ctrl Y", |this| this.undo(true)))
                .child(widgets::separator())
                .child(item("Delete", "Delete", Self::delete_selected)),
            Menu::View => popup
                .child(item("Frame selection", "F", Self::frame_selection))
                .child(item("Show notes pane", "", |this| {
                    this.dock.focus(Pane::Notes);
                }))
                .child(item("Reset view layout", "", |this| this.dock.reset()))
                .child(item("Help", "", |this| this.help = true)),
        }
    }
}

impl DockHost for Demo {
    type Pane = Pane;

    fn dock(&mut self) -> &mut Dock<Pane> {
        &mut self.dock
    }

    fn pane_label(&self, pane: Pane) -> SharedString {
        match pane {
            Pane::Perspective => "Perspective",
            Pane::Top => "Top",
            Pane::Notes => "Notes",
        }
        .into()
    }

    fn render_pane(&mut self, pane: Pane, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match Self::view_index(pane) {
            Some(view) => self.viewport(view, cx),
            None => div()
                .size_full()
                .p_3()
                .bg(theme::get().panel)
                .flex()
                .flex_col()
                .gap_2()
                .child("Panes without a viewport are ordinary GPUI elements.")
                .child(
                    div()
                        .text_color(theme::get().muted)
                        .child("Drag a tab onto another pane to split or group it."),
                )
                .into_any_element(),
        }
    }

    fn pane_focused(&mut self, _: Pane, _: &mut Context<Self>) {
        // A drag in one viewport must not continue in another.
        for view in &mut self.views {
            if let Some(Drag::Gizmo {
                before,
                changed: true,
                ..
            }) = view.drag.take()
            {
                self.history.checkpoint_owned(before);
            }
        }
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_scene();
        window.set_window_title(&format!(
            "Kit demo{}",
            if self.history.dirty() { " *" } else { "" }
        ));
        let docked = dock::render(self, window, cx);
        let mut bar = widgets::menu_bar();
        for (label, menu) in [
            ("File", Menu::File),
            ("Edit", Menu::Edit),
            ("View", Menu::View),
        ] {
            bar = bar.child(
                button(label, self.menu == Some(menu))
                    .h(px(24.))
                    .px_2()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = (this.menu != Some(menu)).then_some(menu);
                        cx.notify();
                    })),
            );
        }
        let toolbar = widgets::toolbar()
            .child(
                button("↶", false)
                    .opacity(if self.history.can_undo() { 1. } else { 0.35 })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.undo(false);
                        cx.notify();
                    })),
            )
            .child(
                button("↷", false)
                    .opacity(if self.history.can_redo() { 1. } else { 0.35 })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.undo(true);
                        cx.notify();
                    })),
            )
            .child(widgets::toolbar_separator())
            .child(
                button("Delete", false).on_click(cx.listener(|this, _, _, cx| {
                    this.delete_selected();
                    cx.notify();
                })),
            );
        let mut root = widgets::window_root("demo")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .child(bar)
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.outliner(cx))
                    .child(div().flex().flex_1().min_w_0().child(docked))
                    .child(self.inspector(cx)),
            )
            .child(widgets::status_bar(
                self.status.clone(),
                self.error,
                format!("{} objects", self.doc.objects.len()),
            ));
        if let Some(menu) = self.menu {
            root = root.child(self.menu_popup(menu, cx));
        }
        if self.help {
            root = root.child(widgets::modal(
                "help",
                widgets::dialog("Kit demo")
                    .child("Every widget, the dock, camera, gizmo and renderer here come from gpui-editor-kit.")
                    .child(button("Close", true).on_click(cx.listener(|this, _, _, cx| {
                        this.help = false;
                        cx.notify();
                    }))),
            ));
        }
        root
    }
}

/// Multiplies a cube's face shade (grey) by an object color.
fn shade(face: u32, color: u32) -> u32 {
    [16, 8, 0]
        .map(|s| ((face >> s) & 255) * ((color >> s) & 255) / 255)
        .into_iter()
        .fold(0, |c, v| c << 8 | v)
}

/// A 24 m checkerboard at y = 0. Lit, so it catches the objects' shadows.
fn ground() -> Mesh {
    let mut data = MeshData {
        material: gpu::Material::Lit,
        casts_shadow: false,
        ..Default::default()
    };
    for z in -12..12 {
        for x in -12..12 {
            let base = data.positions.len() as u32;
            let (x, z) = (x as f32, z as f32);
            data.positions.extend([
                [x, 0., z],
                [x + 1., 0., z],
                [x + 1., 0., z + 1.],
                [x, 0., z + 1.],
            ]);
            let color = if (x + z) as i32 % 2 == 0 {
                0x2c3a44
            } else {
                0x33434e
            };
            data.colors.extend([color; 4]);
            data.normals.extend([[0., 1., 0.]; 4]);
            data.indices.extend([0, 1, 2, 0, 2, 3].map(|i| base + i));
        }
    }
    Mesh::new(data).expect("ground grid")
}

/// Translucent water over one corner of the ground, fading toward the shore.
fn pond() -> Mesh {
    let corners = [[4., 4.], [11., 4.], [11., 11.], [4., 11.]];
    Mesh::new(MeshData {
        positions: corners.map(|[x, z]| [x, 0.25, z]).to_vec(),
        normals: vec![[0., 1., 0.]; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: gpu::Material::Blend(gpu::Blend {
            color: 0x3a8fc4,
            opacity: 0.7,
            fade: 0.5,
            fresnel: 0.6,
            depth: 0.,
        }),
        ..Default::default()
    })
    .expect("pond")
}

pub fn main() -> anyhow::Result<()> {
    platform::configure_display("KIT_DEMO_DISPLAY_BACKEND")?;
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Demo::new(window, cx)),
        )
        .expect("open demo window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    Ok(())
}
