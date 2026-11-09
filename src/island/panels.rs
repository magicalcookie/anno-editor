// The island workspace panes and their input wiring: the tool bar with inline
// brush settings, viewports with load and cursor overlays, the hierarchy and
// scene layers, the ground-material palette and the inspector.
use super::*;
use crate::editor::{Editor, Workspace, empty_pane};
use anno_formats::{assets::Assets, game::ResourceSlots, session::SessionSettings};
use gpui_editor_kit::{
    field::{clear_search, text_field},
    tree,
    widgets::{Disclosure, checkbox, dropdown, hint},
};
use std::collections::BTreeMap;

/// Outliner glyphs of their own: a shoreline (land over a wave), and a
/// plain diamond for objects with no closer picture. Tool icons read as
/// tools here (an anchor said Harbour, a cursor said Select).
const COAST_ICON: Icon = Icon(&[
    &[[3., 10.], [8., 7.], [14., 8.], [21., 5.]],
    &[
        [3., 15.],
        [6., 13.],
        [9., 15.],
        [12., 13.],
        [15., 15.],
        [18., 13.],
        [21., 15.],
    ],
    &[
        [3., 19.],
        [6., 17.],
        [9., 19.],
        [12., 17.],
        [15., 19.],
        [18., 17.],
        [21., 19.],
    ],
]);
const MARKER_ICON: Icon = Icon(&[&[[12., 4.], [19., 12.], [12., 20.], [5., 12.], [12., 4.]]]);
// A closed shore with one wave inside.
const LAKE_ICON: Icon = Icon(&[
    &[
        [4., 12.],
        [6., 7.],
        [12., 5.],
        [18., 7.],
        [20., 12.],
        [18., 17.],
        [12., 19.],
        [6., 17.],
        [4., 12.],
    ],
    &[[8., 12.], [10., 10.5], [12., 12.], [14., 13.5], [16., 12.]],
]);
// Two banks of a bend.
const RIVER_GLYPH: Icon = Icon(&[
    &[[4., 3.], [10., 9.], [6., 15.], [12., 21.]],
    &[[12., 3.], [18., 9.], [14., 15.], [20., 21.]],
]);
// A dashed square: a place set aside for something.
const SLOT_GLYPH: Icon = Icon(&[
    &[[4., 4.], [9., 4.]],
    &[[15., 4.], [20., 4.]],
    &[[20., 4.], [20., 9.]],
    &[[20., 15.], [20., 20.]],
    &[[20., 20.], [15., 20.]],
    &[[9., 20.], [4., 20.]],
    &[[4., 20.], [4., 15.]],
    &[[4., 9.], [4., 4.]],
]);
// A stepped rock face.
const CLIFF_ICON: Icon = Icon(&[&[
    [3., 20.],
    [3., 14.],
    [9., 14.],
    [9., 8.],
    [15., 8.],
    [15., 4.],
    [21., 4.],
]]);

/// Hierarchy groups keyed by (section, display name, guid or file index).
type Groups = BTreeMap<(u8, String, u32), Vec<(Item, [f32; 3])>>;

/// What a Play checklist row's link does.
type CheckAction = Box<dyn Fn(&mut Editor, &mut Context<Editor>)>;

/// Whether a selection box is much lower than it is wide (a 0.6 m bush
/// spread over 2 m): its top and bottom brackets then sit so close that,
/// seen from above, they merged into letter shapes instead of a box.
/// Local axis 1 is up; the corners differ along axis `a` at bit `a`.
fn flat_box(corners: &[Vec3; 8]) -> bool {
    let len = |a: usize| {
        let (p, q) = (corners[0], corners[1 << a]);
        ((0..3).map(|k| (q[k] - p[k]).powi(2)).sum::<f32>()).sqrt()
    };
    len(1) < 0.35 * len(0).min(len(2))
}

/// Corners that carry brackets: all eight, or on a flat box only the four
/// on the ground (bit 1 clear), so the brackets cannot run together.
fn bracket_corners(flat: bool) -> impl Iterator<Item = usize> {
    (0..8).filter(move |i| !flat || i & 2 == 0)
}

/// A ground cover preset's plants rendered for its dropdown pictures, most
/// common first. Grass tufts often come out blank and are skipped, so the
/// three pictures need candidates past the first few: with six, "Game
/// default" (four grasses, three bushes) showed one or two.
const PRESET_PLANTS: usize = 16;

#[derive(Clone)]
struct HierarchyRow {
    action: RowAction,
    depth: usize,
    label: String,
    detail: String,
    disclosure: Disclosure,
    selected: bool,
}

#[derive(Clone, Copy)]
enum RowAction {
    /// The column caption over a group's instances; sorts.
    Caption,
    /// A section a search found nothing in, saying so; does nothing. No
    /// fold and no eye: hiding every object from a row that lists none
    /// changed what the search could not show.
    NoMatches,
    /// An Objects or Props header: folds its section.
    Section(u8),
    Group(Group),
    Item(Item),
}

/// Display name of an asset GUID: English text, else internal name, else
/// the number while assets.xml is still loading.
fn object_name(assets: Option<&Assets>, guid: u32) -> String {
    match assets.and_then(|a| a.get(guid)) {
        Some(a) => a
            .text
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| a.name.clone()),
        None => format!("GUID {guid}"),
    }
}

/// Thumbnail edge in the material palette, logical pixels.
const THUMB: f32 = 72.;

impl Editor {
    pub fn open_island(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(install) = self.install.clone() else {
            return self.message("No install is open", true);
        };
        if let Some(island) = &self.island
            && island.dirty()
        {
            // The same island is already open with its edits; another one
            // asks before they are dropped.
            if island.path != path {
                self.leaving = Some(crate::editor::Leave::Island(path.to_owned()));
            }
            return;
        }
        self.message(format!("Loading {path}…"), false);
        let path = path.to_owned();
        // With assets.xml in, the models load alongside the island.
        let shared = self.assets.clone().map(|a| (install.clone(), a));
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { IslandEditor::load_with(&install, &path, shared) }
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(mut island) => {
                        // An island this project made opens as a new one
                        // again, with its game settings.
                        if let Some(entry) = this.project.islands.iter().find(|i| i.path == path) {
                            island.adopt(entry);
                        }
                        // The name only: region and size head the
                        // Inspector, and a second copy here listed them in
                        // another order.
                        this.message(format!("Opened {}", island_title(&path)), false);
                        island.fit_island();
                        this.island = Some(island);
                        this.switch(Workspace::Island);
                        // Finish the trip Place on island started.
                        if let (Some(cfg), Some(island)) =
                            (this.place_pending.take(), this.island.as_mut())
                        {
                            island.choose_prop(&cfg);
                        }
                    }
                    Err(e) => this.message(format!("Could not open {path}: {e:#}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Terrain → Import heightmap: 16-bit PNG/R16 in game units, 8-bit images
    /// mapped onto −12..40 m, or R32 metres.
    pub fn import_heightmap(&mut self, cx: &mut Context<Self>) {
        self.pick_file("Import heightmap (PNG, R16, R32)", cx, |this, path| {
            if let Some(i) = &mut this.island {
                i.replace_heights(|field| {
                    files::import_heightmap(field, &path, HEIGHT_UNITS, [-12., 40.])?;
                    super::flip_north(field);
                    Ok(format!("Imported heightmap {}", path.display()))
                });
            }
        });
    }

    /// Terrain → Import terrain mesh: OBJ or glTF, fitted to the island.
    pub fn import_terrain_mesh(&mut self, cx: &mut Context<Self>) {
        self.pick_file("Import terrain mesh (OBJ, glTF, GLB)", cx, |this, path| {
            if let Some(i) = &mut this.island {
                i.replace_heights(|field| {
                    // Open sea around the mesh sits at the game's sea floor.
                    let fit = files::MeshFit {
                        floor: -12.,
                        ..Default::default()
                    };
                    let r = files::import_mesh(field, &path, fit)?;
                    Ok(format!(
                        "Imported {} triangles from {}; {:.0}% of the island covered",
                        r.triangles,
                        path.display(),
                        r.covered as f32 * 100. / r.samples.max(1) as f32
                    ))
                });
            }
        });
    }

    /// Terrain → Import tint map: an image of decoded tint colors, mid-grey
    /// neutral, stretched over the island.
    pub fn import_tint(&mut self, cx: &mut Context<Self>) {
        self.pick_file("Import tint map (PNG)", cx, |this, path| {
            if let Some(i) = &mut this.island {
                let world = i.world();
                i.replace_tint(
                    tint::import(&path, world),
                    format!("Imported tint {}", path.display()),
                );
            }
        });
    }

    /// Terrain → Bake tint from terrain: replaces the tint with one derived
    /// from the heights (hollows and steep faces darker).
    pub fn bake_tint(&mut self, cx: &mut Context<Self>) {
        let Some(i) = &mut self.island else {
            return self.message("No island open", true);
        };
        let baked = tint::bake(&i.doc.field, i.world());
        i.replace_tint(Ok(baked), "Baked tint from the terrain".to_owned());
        cx.notify();
    }

    /// Terrain → Export tint map as a PNG of decoded tint colors.
    pub fn export_tint(&mut self, cx: &mut Context<Self>) {
        let Some(island) = &self.island else {
            return self.message("No island open", true);
        };
        let name = island
            .path
            .rsplit('/')
            .next()
            .unwrap_or("island")
            .replace(".a7m", "_tint.png");
        let dir = std::env::home_dir().unwrap_or_else(|| ".".into());
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    match i.doc.tint.as_deref().map(|t| tint::export(t, &path)) {
                        Some(Ok(())) => i.say(format!("Tint written to {}", path.display()), false),
                        Some(Err(e)) => i.say(format!("Export failed: {e:#}"), true),
                        None => i.say("This island has no tint", true),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Terrain → Export heightmap as a 16-bit PNG (or .r16 by extension).
    pub fn export_heightmap(&mut self, cx: &mut Context<Self>) {
        let Some(island) = &self.island else {
            return self.message("No island open", true);
        };
        let name = island
            .path
            .rsplit('/')
            .next()
            .unwrap_or("island")
            .replace(".a7m", "_height.png");
        let dir = std::env::home_dir().unwrap_or_else(|| ".".into());
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    i.export_heightmap(&path);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn pick_file(
        &mut self,
        prompt: &'static str,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, std::path::PathBuf) + 'static,
    ) {
        if self.island.is_none() {
            return self.message("Open an island first", true);
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| {
                then(this, path);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-bakes the ground texture after a paint stroke, off the UI thread.
    /// Strokes that land while a bake runs mark it stale again, so the last
    /// stroke always gets a bake.
    pub fn island_background_work(&mut self, cx: &mut Context<Self>) {
        self.island_models_work(cx);
        self.start_harbour_library(cx);
        self.island_land_work(cx);
        let Some(job) = self
            .island
            .as_mut()
            .filter(|i| i.ground_stale && !i.baking)
            .and_then(|i| {
                i.baking = true;
                i.bake_job()
            })
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let baked = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    i.baking = false;
                    match baked {
                        Ok(texture) => i.set_ground(texture),
                        Err(e) => i.say(format!("Ground bake failed: {e:#}"), true),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Lets the land grids follow a released stroke off the UI thread, one
    /// job at a time; strokes released meanwhile get the next.
    fn island_land_work(&mut self, cx: &mut Context<Self>) {
        let Some((job, work)) = self.island.as_mut().and_then(|i| i.land_job()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let changes = cx.background_executor().spawn(async move { work() }).await;
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    i.land_synced(job, changes);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Streams the island's models in batches. assets.xml arriving later
    /// makes objects resolvable, so it is handed over here first.
    fn island_models_work(&mut self, cx: &mut Context<Self>) {
        let (Some(island), Some(install)) = (&mut self.island, self.install.clone()) else {
            return;
        };
        if island.assets.is_none() && self.assets.is_some() {
            island.assets = self.assets.clone();
            island.scene_stale = true;
        }
        let Some(job) = island.models_job(install) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let loaded = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    i.models_loaded(loaded);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The floating tool panel over the viewport: tool icons, and
    /// underneath the active tool's options.
    fn island_tool_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let island = self
            .island
            .as_ref()
            .expect("viewport renders with an island");
        // Tools in captioned groups, so the strip reads before it is learnt.
        // Select stands alone and needs no caption; the others name what
        // their tools work on rather than repeating a tool's name.
        let group_of = |tool: Tool| match tool {
            Tool::Select => "Edit",
            Tool::Paint | Tool::Tint => "Surface",
            Tool::Place | Tool::Scatter => "Objects",
            Tool::Grid | Tool::Harbour | Tool::Slot | Tool::River => "Gameplay",
            _ => "Sculpt",
        };
        let mut icons = div()
            .id("island-tools")
            .flex()
            .px_1()
            .pt(px(2.))
            .pb_1()
            .gap_1()
            .items_start();
        for (n, caption) in ["Edit", "Sculpt", "Surface", "Objects", "Gameplay"]
            .into_iter()
            .enumerate()
        {
            // Tight pitch: twenty tools and their groups must fit a 1600
            // px window with the panel's gutters on both sides.
            let mut row = div().flex().gap(px(1.));
            for (tool, icon, label, key, detail) in Tool::ALL {
                if group_of(tool) != caption {
                    continue;
                }
                // Icon and name: the strip reads without hovering each glyph.
                let on = island.tool == tool;
                row = row.child(
                    widgets::tool_button(
                        SharedString::from(format!("tool-{label}")),
                        icon,
                        // One line under every icon; the tooltip has the rest.
                        match label {
                            "Harbour spot" => "Harbour",
                            "Resource slot" => "Resources",
                            "River & lake" => "Rivers",
                            other => other,
                        },
                        on,
                    )
                    .tooltip(Tooltip::text(label, key, detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.tool = tool;
                        }
                        cx.notify();
                    })),
                );
            }
            if n > 0 {
                icons = icons.child(widgets::toolbar_separator().mt(px(18.)));
            }
            icons = icons.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        // Quieter than the tool names under them (at one
                        // size the strip read as two rows of labels), but
                        // muted rather than disabled: they name real groups.
                        div()
                            .pl_1()
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::get().muted)
                            .child(caption),
                    )
                    .child(row),
            );
        }
        // Camera commands with names, not only F: the island, or what is
        // selected.
        let has_selection = !island.selection.is_empty();
        // Icon and caption like the tools beside them; Frame dims without a
        // selection and its tooltip says why.
        let camera = div()
            .flex()
            .gap_1()
            .child(
                widgets::tool_button("camera-fit", Icon::FIT, "Fit", false)
                    .tooltip(Tooltip::text(
                        "Fit island",
                        if has_selection { "" } else { "F" },
                        "Frame the land in both views",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.fit_island();
                        }
                        cx.notify();
                    })),
            )
            .child(if has_selection {
                widgets::tool_button("camera-frame", Icon::FRAME, "Frame", false)
                    .tooltip(Tooltip::text(
                        "Frame selection",
                        "F",
                        "Zoom both views onto the selection",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.frame();
                        }
                        cx.notify();
                    }))
            } else {
                widgets::tool_button("camera-frame", Icon::FRAME, "Frame", false)
                    .opacity(0.4)
                    .cursor_default()
                    .tooltip(Tooltip::text(
                        "Frame selection",
                        "F",
                        "Select something first",
                    ))
            });
        // The active tool's settings, as the panel's second row under a
        // rule: the highlighted icon above says whose they are.
        let mut options = div()
            .id("island-tool-options")
            .flex()
            .items_center()
            // As tall as the slider row (28px fields and the padding), so
            // the panel's edge stays put when the tool changes; 46 left
            // 10px of slack over the viewport.
            .min_h(px(36.))
            .px_2()
            .py_1()
            .gap_1()
            .flex_wrap()
            .border_t_1()
            .border_color(theme::get().floating_border);
        match island.tool {
            Tool::Select => {
                let idle = island.selection.is_empty();
                let row = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(widgets::segmented_keys(
                        "gizmo-mode",
                        [
                            (gizmo::Mode::Move, "Move".into(), "W"),
                            (gizmo::Mode::Rotate, "Rotate".into(), "E"),
                            (gizmo::Mode::Scale, "Scale".into(), "R"),
                        ],
                        &island.gizmo,
                        cx,
                        |this, mode, _, _| {
                            if let Some(i) = &mut this.island {
                                i.gizmo = mode;
                            }
                        },
                    ))
                    // Snapping existed with nothing on screen saying so. A
                    // move's step is a choice: small props need less than
                    // a metre, terrain features more.
                    .child(
                        div()
                            .px_2()
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            .child(match island.gizmo {
                                // "Hold": the picked step is a setting,
                                // snapping happens only while Ctrl is down.
                                gizmo::Mode::Move => "Hold Ctrl to snap to",
                                gizmo::Mode::Rotate => "Hold Ctrl to snap to 15°",
                                gizmo::Mode::Scale => "Hold Ctrl to snap to 0.1×",
                            }),
                    )
                    .when(island.gizmo == gizmo::Mode::Move, |d| {
                        d.child(widgets::segmented(
                            "snap-step",
                            [
                                (0.1f32, "0.1 m".into()),
                                (0.5, "0.5 m".into()),
                                (1., "1 m".into()),
                            ],
                            &island.snap_step,
                            cx,
                            |this, step, _, _| {
                                if let Some(i) = &mut this.island {
                                    i.snap_step = step;
                                }
                            },
                        ))
                    });
                // The row keeps its shape with nothing selected: a sentence
                // in its place changed the panel's height between tools and
                // hid the mode until a click. The mode can be set first, as
                // in Blender and Unreal; the note at the end says what the
                // row waits for.
                options = options.child(row.when(idle, |d| {
                    d.child(
                        div()
                            .px_2()
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            // The armed mode's verb: Rotate set first read
                            // "to move it".
                            .child(match island.gizmo {
                                gizmo::Mode::Move => "Click an object or prop to move it",
                                gizmo::Mode::Rotate => "Click an object or prop to rotate it",
                                gizmo::Mode::Scale => "Click an object or prop to scale it",
                            }),
                    )
                }));
            }
            Tool::Harbour => options = self.island_harbour_options(options, cx),
            Tool::Slot => options = self.island_slot_options(options, cx),
            Tool::River => options = self.island_river_options(options, cx),
            Tool::Place => {
                let library_open = self.docks[self.dock_index()]
                    .visible_panes()
                    .contains(&crate::editor::Pane::IslandLibrary);
                // Only asked on the This mod tab: it reads the project's files.
                let mod_empty = library_open
                    && island.library_tab == super::catalog::Kind::Mod
                    && self.model_entries().is_empty();
                let name = match &island.place_object {
                    Some((_, Some(config))) => Some(pretty(mesh_name(config))),
                    Some((t, None)) => Some(object_name(self.assets.as_deref(), t.object.guid)),
                    None => island
                        .place
                        .and_then(|f| island.doc.meshes.get(f as usize))
                        .map(|m| pretty(mesh_name(m))),
                };

                options = match name {
                    Some(name) => options.child(
                        div()
                            .px_2()
                            .text_size(px(11.))
                            .child(format!("Placing {name}")),
                    ),
                    // A way to the models, not only a pointer to them.
                    // Pointing at the Library while it shows; a way to it
                    // while it is folded away.
                    // The Library owns its own empty state; this only points.
                    // The row stays, so the panel keeps one height across
                    // tools, and holds the instruction beside its tool.
                    None if library_open => options.child(
                        div()
                            .px_2()
                            .flex()
                            .gap_1()
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            // One sentence on every tab, as the tool's
                            // instruction; two wordings for one tool read as
                            // two tools. The arrow by "below", where it
                            // points from. An empty This mod's way to Props
                            // is the Library's own empty state, right under.
                            .child(if mod_empty {
                                "Add a model below ↓ or pick one in"
                            } else {
                                // The whole sequence: picking alone did not say
                                // the next click lands on the island.
                                // The variant groups are explained on their own
                                // badges: in the hint it was wrong on tabs that
                                // have none.
                                "Click a model below ↓, then click the island, or drag one onto it"
                            })
                            // "Props" as the tab it names, one click away; no
                            // arrow: the tab is below, beside This mod.
                            .when(mod_empty, |d| {
                                d.child(
                                    div()
                                        .id("place-switch-props")
                                        .text_color(theme::get().accent)
                                        .cursor_pointer()
                                        .hover(|s| s.underline())
                                        .child("Props")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.browse_game_props();
                                            cx.notify();
                                        })),
                                )
                            }),
                    ),
                    None => options
                        .child(
                            div()
                                .px_2()
                                .text_size(px(11.))
                                .child("Pick a model to place"),
                        )
                        .child(
                            widgets::action("Open Library", widgets::Tone::Secondary)
                                .h(px(26.))
                                .text_size(px(11.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_pane(crate::editor::Pane::IslandLibrary);
                                    cx.notify();
                                })),
                        ),
                };
            }
            _ => {
                // The caption says what the number means in the current mode,
                // so the mode tooltip is not the only place that explains it.
                let (strength, unit) = if matches!(island.tool, Tool::Raise | Tool::Lower) {
                    match island.brush.mode {
                        Mode::Absolute => ("Max per stroke", "m"),
                        _ => ("Height per dab", "m"),
                    }
                } else {
                    ("Strength", "")
                };
                options = options
                    .child(self.island_compact(Field::Radius, "Radius", "m", cx))
                    .child(self.island_compact(Field::Inner, "Inner radius", "%", cx))
                    .child(self.island_compact(Field::Strength, strength, unit, cx));
                // A caption, then the choice: modes and falloff read as two
                // settings, not one row of loose buttons.
                if matches!(island.tool, Tool::Raise | Tool::Lower) {
                    // A plain muted caption beside its segments, as "Ctrl snaps
                    // to" sits beside its: inside a frame flush with them it
                    // read as a third choice. In-frame labels are for fields
                    // that hold one value (sliders, Falloff).
                    options = options.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .pl_2()
                                    .text_size(px(11.))
                                    .text_color(theme::get().muted)
                                    .child("Buildup"),
                            )
                            .child(
                        widgets::segmented(
                            "brush-mode",
                            [
                                (Mode::Standard, "Accumulate".into()),
                                (Mode::Absolute, "Limit per stroke".into()),
                            ],
                            &island.brush.mode,
                            cx,
                            |this, mode, _, _| {
                                if let Some(i) = &mut this.island {
                                    i.brush.mode = mode;
                                }
                            },
                        )
                        .tooltip(Tooltip::text(
                            "Mode",
                            "",
                            "Accumulate: every dab adds its height, so passes keep building. Limit per stroke: one stroke moves ground at most that far from where it started",
                        )),
                            ),
                    );
                }
                if matches!(island.tool, Tool::Flatten | Tool::Fill | Tool::Scrape) {
                    options = options.child(self.island_compact(Field::Level, "Target", "m", cx));
                }
                if island.tool == Tool::Paint {
                    options = options.child(
                        div()
                            .px_1()
                            .text_size(px(11.))
                            .child(format!("Material {}", island.paint)),
                    );
                }
                if island.tool == Tool::Grid {
                    options = self.island_grid_options(options, cx);
                }
                if island.tool == Tool::Scatter {
                    options = self.island_scatter_options(options, cx);
                }
                if island.tool == Tool::Tint {
                    options = self.island_tint_options(options, cx);
                }
                // Shaped as the sliders beside it, its label inside: a caption
                // outside and a shorter box read as a second kind of control.
                options = options.child(
                    widgets::field_dropdown(
                        "brush-falloff",
                        "Falloff",
                        // "Smooth" is also a tool; the curve is a soft one.
                        Falloff::ALL
                            .map(|f| {
                                let name = match format!("{f:?}").as_str() {
                                    "Smooth" => "Soft".to_owned(),
                                    other => other.to_owned(),
                                };
                                (f, name.into())
                            })
                            .to_vec(),
                        &island.brush.falloff,
                        window,
                        cx,
                        |this: &mut Editor, falloff, _, _| {
                            if let Some(i) = &mut this.island {
                                i.brush.falloff = falloff;
                            }
                        },
                    )
                    // As wide as its label and longest choice, so the arrow
                    // sits by the value.
                    .w(px(128.)),
                );
            }
        }
        // The row always shows: it ends with the camera commands, pushed to
        // the right edge.
        // Fit and Frame end the icon row, after a rule: camera commands,
        // not tool settings, and fixed in place whichever tool is active.
        icons = icons.child(widgets::toolbar_separator().mt(px(18.))).child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .pl_1()
                        .text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::get().muted)
                        .child("View"),
                )
                .child(camera),
        );
        // One panel floating over the viewport: icons, then the settings
        // across its full width.
        div().flex().child(
            widgets::floating_surface("island-tools-panel")
                .flex_col()
                .items_stretch()
                .child(icons)
                .child(options),
        )
    }

    /// Grid tool: which layer, and the value painted (legend or number).
    fn island_grid_options(
        &self,
        mut options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        if island.doc.grids.is_none() {
            return options.child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .child("No grids on this island"),
            );
        }
        let mut layers = div().flex().flex_wrap().gap_1().max_w(px(560.));
        for kind in grids::PAINTABLE {
            if island
                .doc
                .grids
                .as_ref()
                .is_none_or(|g| g.layer(kind).is_none())
            {
                continue;
            }
            layers = layers.child(
                button(grids::label(kind), island.grid == kind)
                    .text_size(px(11.))
                    .h(px(25.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.grid = kind;
                            // Two-entry legends paint their "on" value.
                            i.grid_value = match kind.legend(i.game).as_slice() {
                                [_, (on, _)] => *on,
                                _ => i.grid_value.min(grids::max_value(i.game, kind)).max(1),
                            };
                        }
                        cx.notify();
                    })),
            );
        }
        options = options.child(layers);
        if island.grid == anno_formats::grids::Kind::WaterFlow {
            return options.child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .child("Drag to comb the current along the stroke; Shift calms it"),
            );
        }
        if island.grid == anno_formats::grids::Kind::AreaIds {
            return options
                .child(
                    div()
                        .px_1()
                        .text_size(px(11.))
                        .child("Paints “Buildable”, Shift paints “Not buildable”"),
                )
                .child(
                    button("Auto from slope", false)
                        .text_size(px(11.))
                        .h(px(25.))
                        .tooltip(Tooltip::text(
                            "Auto from slope",
                            "",
                            "Buildable up to 15°, as on shipped islands. Replaces painted areas",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(i) = &mut this.island {
                                i.checkpoint();
                                i.derive_areas_from_slope();
                                i.say("Buildable land follows the slope (up to 15°)", false);
                            }
                            cx.notify();
                        })),
                );
        }
        let auto = match island.grid {
            Kind::Trees => Some((
                "Auto from trees",
                "Tree cells under tree props on buildable land, then forest chunks. Replaces painted cells",
            )),
            Kind::ForestChunks => Some((
                "Auto chunks",
                "Renumber forest chunks from the tree cells: up to 10, spread over the island",
            )),
            Kind::Environment => Some((
                "Auto from terrain",
                "Sea, coast, forest, mountain, marsh and meadow from land, slope, height and tree cells. Replaces painted types",
            )),
            Kind::Herds => Some((
                "Auto from land",
                "Buildable land outside tree cells and rivers, as on shipped islands. Replaces painted cells",
            )),
            _ => None,
        };
        let derive = |kind: Option<Kind>| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                if let Some(i) = &mut this.island {
                    i.checkpoint();
                    let text = i.derive_grids(kind);
                    i.say(text, false);
                }
                cx.notify();
            })
        };
        let mut actions = div().flex().gap_1();
        if let Some((name, tip)) = auto {
            actions = actions.child(
                button(name, false)
                    .text_size(px(11.))
                    .h(px(25.))
                    .tooltip(Tooltip::text(name, "", tip))
                    .on_click(derive(Some(island.grid))),
            );
        }
        options = options.child(
            actions.child(
                button("Auto-fill gameplay grids", false)
                    .text_size(px(11.))
                    .h(px(25.))
                    .tooltip(Tooltip::text(
                        "Auto-fill gameplay grids",
                        "",
                        "Tree cells, forest chunks, environment and herds from the terrain and tree props, in one undo step",
                    ))
                    .on_click(derive(None)),
            ),
        );
        let legend = island.grid.legend(island.game);
        if legend.len() > 2 {
            // Long legends (environment types): a numbered field plus the name.
            let name = legend
                .iter()
                .find(|(v, _)| *v == island.grid_value)
                .map_or("", |(_, n)| n);
            options = options
                .child(self.island_compact(Field::GridValue, "Value", "", cx))
                .child(div().px_1().text_size(px(11.)).child(name.to_owned()));
        } else if legend.is_empty() {
            options = options.child(self.island_compact(Field::GridValue, "Value", "", cx));
        } else {
            options = options.child(div().px_1().text_size(px(11.)).child(format!(
                "Paints “{}”, Shift paints “{}”",
                legend[1].1, legend[0].1
            )));
        }
        options
    }

    /// Scatter tool: the prop set, density and variation.
    fn island_scatter_options(
        &self,
        options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        // The set as thumbnail chips; × takes one out.
        let mut set = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .max_w(px(620.));
        for (k, &file) in island.scatter.files.iter().enumerate() {
            let path = island
                .doc
                .meshes
                .get(file as usize)
                .map(|m| m.replace('\\', "/"))
                .unwrap_or_default();
            let name = mesh_name(&path).to_owned();
            let picture = self.thumbs.as_ref().and_then(|t| t.get(&path));
            set = set.child(
                div()
                    .id(("scatter-chip", k))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(2.))
                    .pr_1()
                    .h(px(25.))
                    .rounded_sm()
                    .bg(theme::get().panel)
                    .border_1()
                    .border_color(theme::get().border)
                    .tooltip(Tooltip::text(
                        name.clone(),
                        "",
                        "Click to take it out of the set",
                    ))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::get().hover))
                    .child(match picture {
                        Some(p) => img(p).size(px(21.)).into_any_element(),
                        None => div().size(px(21.)).into_any_element(),
                    })
                    .child(
                        div()
                            .max_w(px(90.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(10.))
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            .child("×"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.scatter.files.remove(k);
                        }
                        cx.notify();
                    })),
            );
        }
        if island.scatter.files.is_empty() {
            set = set.child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("Empty set: click props in the Library, or start from"),
            );
        } else {
            set = set.child(
                button("Clear", false)
                    .text_size(px(11.))
                    .h(px(25.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.scatter.files.clear();
                        }
                        cx.notify();
                    })),
            );
        }
        // Starter sets: this region's most used shipped props of a kind.
        for (label, tip) in [
            (
                "Trees",
                "The region’s six most used harvestable trees; marks tree cells",
            ),
            ("Bushes", "The region’s six most used bushes"),
            ("Rocks", "The region’s six most used rocks"),
        ] {
            set = set.child(
                button(label, false)
                    .text_size(px(11.))
                    .h(px(25.))
                    .tooltip(Tooltip::text(label, "", tip))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scatter_preset(label);
                        cx.notify();
                    })),
            );
        }
        let marks = island.scatter.mark_trees;
        options
            .child(set)
            .child(self.island_compact(Field::Density, "Per 100 m²", "", cx))
            .child(self.island_compact(Field::Jitter, "Scale ±", "%", cx))
            .child(
                button("Mark tree cells", marks)
                    .text_size(px(11.))
                    .h(px(25.))
                    .tooltip(Tooltip::text(
                        "Mark tree cells",
                        "",
                        "Also set the RegrowManager tree cells under new props, so they are harvestable",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.scatter.mark_trees = !i.scatter.mark_trees;
                        }
                        cx.notify();
                    })),
            )
            .child(self.island_cliff_options(cx))
    }

    /// Auto cliffs sits with the scatter tool: it is a scatter over every
    /// steep face, with the region's cliff set instead of the brush's.
    fn island_cliff_options(&self, cx: &mut Context<Self>) -> Div {
        let island = self.island.as_ref().expect("options render with an island");
        let step = |delta: f32| {
            cx.listener(
                move |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                    if let Some(i) = &mut this.island {
                        i.cliffs.density = (i.cliffs.density + delta).clamp(0.5, 2.);
                    }
                    cx.notify();
                },
            )
        };
        let replace = island.cliffs.replace;
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                button("Auto cliffs", false)
                    .text_size(px(11.))
                    .h(px(25.))
                    .tooltip(Tooltip::text(
                        "Auto cliffs",
                        "",
                        "Cliff walls and big rocks of the island’s region on every face over 35°, turned and sunk like on shipped islands. One undo step",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(install) = this.install.clone() else {
                            return this.message("No install is open", true);
                        };
                        if let Some(i) = &mut this.island {
                            i.auto_cliffs(&install);
                        }
                        cx.notify();
                    })),
            )
            .child(button("−", false).text_size(px(11.)).on_click(step(-0.25)))
            .child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .child(format!("density ×{:.2}", island.cliffs.density)),
            )
            .child(button("+", false).text_size(px(11.)).on_click(step(0.25)))
            .child(
                button("Replace earlier", replace)
                    .text_size(px(11.))
                    .h(px(25.))
                    .tooltip(Tooltip::text(
                        "Replace earlier",
                        "",
                        "A rerun removes the pieces Auto cliffs placed before; off, it only fills gaps",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.cliffs.replace = !i.cliffs.replace;
                        }
                        cx.notify();
                    })),
            )
    }

    /// Tint tool: color presets and the current target.
    fn island_tint_options(
        &self,
        mut options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        for (name, color) in tint::PRESETS {
            options = options.child(
                button(name, island.tint_color == color)
                    .text_size(px(11.))
                    .h(px(25.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.tint_color = color;
                        }
                        cx.notify();
                    })),
            );
        }
        let [r, g, b] = island.tint_color;
        let swatch = |v: f32| ((v / 2.).clamp(0., 1.) * 255.) as u32;
        options.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .text_size(px(11.))
                .child(
                    div()
                        .size(px(14.))
                        .rounded_sm()
                        .bg(rgb(swatch(r) << 16 | swatch(g) << 8 | swatch(b))),
                )
                .child(format!("×{r:.2} ×{g:.2} ×{b:.2}")),
        )
    }

    fn island_compact(
        &self,
        field: Field,
        label: &'static str,
        unit: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self
            .island
            .as_ref()
            .expect("tool options render with an island");
        let editing = island
            .editing
            .as_ref()
            .filter(|e| e.field == field)
            .map(|e| e.display());
        let value = match (field, island.level) {
            (Field::Level, None) => "stroke".to_owned(),
            _ => {
                let v = island.field_value(field);
                // Fewer decimals where they are noise. Positions keep one,
                // as the cursor readout does, so X, Y and Z line up.
                // A space before metres, as text is written; none before
                // % or ° or ×.
                let unit = if unit == "m" { " m" } else { unit };
                // Whole degrees, as a prop's "−22°", and no sign on a value
                // that rounds to zero: "−0.00°" read as a fault.
                let shown = |v: f32, places: usize| {
                    let text = format!("{v:.places$}");
                    let zero = text
                        .trim_start_matches('-')
                        .chars()
                        .all(|c| c == '0' || c == '.');
                    if zero {
                        text.trim_start_matches('-').to_owned()
                    } else {
                        text
                    }
                };
                if matches!(field, Field::Position(_)) {
                    format!("{}{unit}", shown(v, 1))
                } else if v.abs() >= 10.
                    || matches!(
                        field,
                        Field::GridValue | Field::Jitter | Field::Inner | Field::Yaw
                    )
                {
                    format!("{}{unit}", shown(v, 0))
                } else {
                    format!("{}{unit}", shown(v, 2))
                }
            }
        };
        let id = SharedString::from(format!("compact-{label}"));
        // A true minus sign, as typeset numbers use; typing still takes "-".
        let text = editing.clone().unwrap_or_else(|| value.replace('-', "−"));
        let element = match island.field_fraction(field).filter(|_| editing.is_none()) {
            // A slider: drag across to set, click to type, scroll to nudge.
            Some(fraction) => {
                let me = id.clone();
                gpui_editor_kit::field::slider_field(id, label, text, fraction, false)
                    // What the control takes, since the track alone did not say.
                    .tooltip(Tooltip::text(
                        label,
                        "",
                        "Drag to adjust · Click to type · Scroll to nudge",
                    ))
                    .on_drag_move(cx.listener(
                        move |this,
                              e: &DragMoveEvent<gpui_editor_kit::field::SliderDrag>,
                              _,
                              cx| {
                            if e.drag(cx).0 != me {
                                return;
                            }
                            let f = gpui_editor_kit::field::slider_fraction(e);
                            if let Some(i) = &mut this.island {
                                i.set_field_fraction(field, f);
                            }
                            cx.notify();
                        },
                    ))
            }
            None => gpui_editor_kit::field::compact_field(id, label, text, editing.is_some()),
        };
        element
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                if let Some(i) = &mut this.island {
                    i.editing = Some(TextEdit::new(
                        field,
                        format!("{:.2}", i.field_value(field)),
                        Filter::Number,
                    ));
                }
                cx.notify();
            }))
            .on_scroll(cx.listener(move |this, e: &ScrollEvent, _, cx| {
                // Scrolling over a field nudges it.
                let Some(i) = &mut this.island else { return };
                let dy = match e.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                    ScrollDelta::Lines(p) => p.y * 32.,
                };
                let v = i.field_value(field);
                let step = match field {
                    Field::Radius | Field::Density => v * 0.1,
                    Field::Strength => 0.05,
                    Field::GridValue | Field::Jitter => 1.,
                    _ => 0.25,
                };
                i.set_field(field, v + dy.signum() * step.max(0.01));
                cx.notify();
            }))
    }

    pub fn render_island_view(
        &mut self,
        view: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.island_background_work(cx);
        let Some(island) = &mut self.island else {
            return widgets::empty_state_icon(
                gpui_editor_kit::icon::Icon::ISLAND,
                "No island open",
            )
            .child(hint(
                "Click an island on the left to open it, or start your own.",
            ))
            // Where the preview appears, said apart: in brackets it hid.
            .child(
                hint("Point at one to preview it in the Inspector.").text_color(theme::get().muted),
            )
            .child(
                div()
                    .flex()
                    .pt_2()
                    .gap_2()
                    .child(
                        // One filled button on screen: while an island is
                        // previewed its Open is, so New steps down; with
                        // both filled the eye went to creating while the
                        // pointer said opening.
                        widgets::action(
                            "New island…",
                            if self.island_peek.is_some() {
                                widgets::Tone::Secondary
                            } else {
                                widgets::Tone::Primary
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_wizard();
                            cx.notify();
                        })),
                    )
                    .child(
                        widgets::action(self.import_label(), widgets::Tone::Secondary).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.open_import(cx);
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element();
        };
        island.refresh_scene();
        island.push_overlay();
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let tools = self.island_tool_bar(window, cx);
        let Some(island) = &self.island else {
            return empty_pane("No island open");
        };
        let mut root = div()
            .id(("island-view", view))
            .relative()
            .size_full()
            .bg(theme::get().viewport)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let moved = entity.update(cx, |this, _| {
                            this.island.as_mut().is_some_and(|i| {
                                std::mem::replace(&mut i.views[view].bounds, bounds) != bounds
                            })
                        });
                        // The hint and readouts over this view are laid
                        // out from these bounds at the window root, before
                        // this prepaint; a view that just appeared (the Top
                        // tab) needs one more frame to get them.
                        if moved {
                            let entity = entity.clone();
                            window
                                .on_next_frame(move |_, cx| entity.update(cx, |_, cx| cx.notify()));
                        }
                    },
                    move |bounds, _, window, cx| {
                        let this = paint_entity.read(cx);
                        let Some(island) = &this.island else { return };
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            island.paint_view(view, bounds, window);
                        });
                    },
                )
                .size_full(),
            );
        // Assigned test-map resources next to their slots. Positions use the
        // bounds of the last paint, so they trail a camera move by a frame.
        if island.tool == Tool::Slot {
            let v = &island.views[view];
            let extent = island.extent();
            for (at, label) in island.slot_labels() {
                let p = v
                    .camera
                    .project([at[0], at[1] + 2., at[2]], v.bounds, extent);
                root = root.child(
                    div()
                        .absolute()
                        .left(p.x - v.bounds.origin.x + px(6.))
                        .top(p.y - v.bounds.origin.y - px(8.))
                        .px_1()
                        .rounded(px(3.))
                        .bg(rgba(crate::colors::LABEL_BG))
                        .text_size(px(10.))
                        .text_color(rgb(crate::colors::PICK))
                        .child(label),
                );
            }
        }
        // The hovered Hierarchy row's ID on its white box, and a single
        // selection's on its orange one: on bare grass a box alone read as
        // empty, and a sparse bush under its gizmo could not be told apart
        // from the grass around it.
        let hovered = island
            .hover_item
            .filter(|i| !island.selection.contains(i))
            .map(|i| (i, crate::colors::HANDLE));
        let single = (island.selection.len() == 1)
            .then(|| island.selection.iter().next().copied())
            .flatten()
            .map(|i| (i, crate::colors::SELECTION_LABEL));
        for (item, corners, color) in [hovered, single]
            .into_iter()
            .flatten()
            .filter_map(|(i, c)| Some((i, island.item_box(i)?, c)))
        {
            let v = &island.views[view];
            let project = |c: [f32; 3]| v.camera.project(c, v.bounds, island.extent());
            let n = corners.len() as f32;
            let mid = |a: usize| corners.iter().map(|c| c[a]).sum::<f32>() / n;
            // A hover box's ID on its top edge: it has no gizmo to dodge.
            // A selection's beside the gizmo's pivot, opposite its three
            // axes: the gizmo keeps one screen size however close the box
            // is, so any spot on the box (its top, a corner) landed on an
            // axis at some zoom or angle, and the space between the axes'
            // backs is the one the gizmo never covers.
            let ceiling = corners.iter().map(|c| c[1]).fold(f32::MIN, f32::max);
            let top = project([mid(0), ceiling, mid(2)]);
            let pivot = island.doc.matrix(item).map(|m| math::translation(&m));
            let p = match pivot {
                Some(o) if color != crate::colors::HANDLE => {
                    let at = project(o);
                    let (mut sx, mut sy) = (0f32, 0f32);
                    for a in 0..3 {
                        let mut tip = o;
                        tip[a] += 1.;
                        let t = project(tip);
                        let (dx, dy) = (f32::from(t.x - at.x), f32::from(t.y - at.y));
                        let len = dx.hypot(dy).max(1e-3);
                        sx += dx / len;
                        sy += dy / len;
                    }
                    let len = sx.hypot(sy);
                    // Axes summing to nothing (seen end-on): up and left.
                    let (ux, uy) = if len > 0.2 {
                        (-sx / len, -sy / len)
                    } else {
                        (-0.7, -0.7)
                    };
                    // The label's middle 40 px out, near enough to read as
                    // the pivot's (70 floated it over the next bush); it is
                    // drawn from 16 px left of `p` and 22 px above it.
                    at + point(px(ux * 40.), px(uy * 40. + 14.))
                }
                // Lifted clear of the top brackets: on the box's top edge the
                // tag sat on its corners.
                _ => top - point(px(0.), px(12.)),
            };
            let id = match item {
                Item::Prop(id) | Item::Object(id) => id & 0xffff_ffff,
            };
            // A hovered row's model named too: pointing down the list at a
            // neighbour, "#757" alone needed a lookup. The selection's stays
            // short; the Inspector names it.
            let label = match island
                .item_cfg(item)
                .filter(|_| color == crate::colors::HANDLE)
            {
                Some(cfg) => format!("{} · #{id}", pretty(mesh_name(&cfg))),
                None => format!("#{id}"),
            };
            root = root.child(
                div()
                    .absolute()
                    .left(p.x - v.bounds.origin.x - px(16.))
                    .top(p.y - v.bounds.origin.y - px(22.))
                    .px_1()
                    .rounded(px(3.))
                    .bg(rgba(crate::colors::LABEL_BG))
                    .text_size(px(11.))
                    .text_color(rgb(color))
                    .whitespace_nowrap()
                    .child(label),
            );
        }
        // Whose file this is and where a save puts it.
        if !self.stats {
            let ours = self.project.islands.iter().any(|i| i.path == island.path);
            let file = island.path.rsplit('/').next().unwrap_or("").to_owned();
            let name = island_title(&island.path);
            root = root.child(
                widgets::floating_surface(("island-document", view))
                    .tooltip(Tooltip::text(
                        name.clone(),
                        "",
                        if ours {
                            format!("{file}, in this mod")
                        } else {
                            format!("{file}. A game original: saving puts your edits in this mod")
                        },
                    ))
                    .absolute()
                    // Bottom right, on the hint pill's line: the tool strip
                    // owns the top edge.
                    .bottom_2()
                    .right_3()
                    .px_2()
                    .py(px(2.))
                    // No gap: the kit's middots space the parts.
                    .text_size(px(11.))
                    // The island's name first: with a prop selected the
                    // Inspector and the status bar name the prop, and
                    // nothing on screen said which island was open.
                    // One frame: a tinted badge inside the pill read as two
                    // nested boxes. Coloured words with a dot instead.
                    .child(div().text_color(theme::get().text).child(name.clone()))
                    // The region too: with a prop selected the Inspector
                    // names the prop, and "Medium 01" alone is three
                    // islands. Its game name, as the Inspector heading says it.
                    .when_some(
                        island
                            .path
                            .split("/provinces/")
                            .nth(1)
                            .and_then(|r| r.split('/').next())
                            .map(wizard::province_short),
                        |d, region| {
                            d.child(widgets::middot())
                                .child(div().text_color(theme::get().muted).child(region))
                        },
                    )
                    .when(!ours, |d| d.child(widgets::middot()))
                    .child(if ours {
                        div()
                            .ml_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(theme::get().accent)
                            .child(div().size(px(6.)).rounded_full().bg(theme::get().accent))
                            .child("Project island")
                            .into_any_element()
                    } else {
                        // Tinted, not filled: a permanent status beside the orange
                        // selection, not an alert.
                        // While the Inspector is open its "Add island to"
                        // button is the way in, and the chip only says whose
                        // island this is: two buttons for one command on one
                        // screen. With the Inspector closed the chip is the
                        // way in, said as an action.
                        // Under a selection the folded settings band keeps
                        // its own Add link, so the chip stays a label then too.
                        let inspector = self
                            .dock_visible_panes()
                            .contains(&crate::editor::Pane::IslandInspector);
                        let chip = div()
                            .id("chip-game-original")
                            .rounded(px(3.))
                            .flex()
                            .items_center()
                            .gap_2()
                            // Grey: where the island comes from, not a
                            // warning. Amber is the outdated-copy warning.
                            .text_color(theme::get().muted)
                            // After a middot, not a dot: dots elsewhere
                            // mean attention (amber) or on this island (blue).
                            .child("Game original");
                        if inspector {
                            chip.tooltip(Tooltip::text(
                                format!("Not in ‘{}’ yet", self.project.info.name),
                                "Ctrl S",
                                "Add it from the Inspector, or press Ctrl S",
                            ))
                        } else {
                            chip.cursor_pointer()
                                .hover(|s| s.opacity(0.85))
                                .child(
                                    div()
                                        .ml_1()
                                        .text_color(theme::get().accent)
                                        // The Inspector button's words: one
                                        // name for one action.
                                        .child(format!(
                                            "Add island to ‘{}’",
                                            self.project.info.name
                                        )),
                                )
                                .tooltip(Tooltip::text(
                                    format!("Not in ‘{}’ yet", self.project.info.name),
                                    "Ctrl S",
                                    "Click to add this island to the mod",
                                ))
                                // Not a viewport press: the click is the chip's.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(i) = &mut this.island {
                                        i.want_in_mod();
                                    }
                                    this.save();
                                    cx.notify();
                                }))
                        }
                        .into_any_element()
                    }),
            );
        }
        if self.stats && island.show_log && view == 0 && !island.load_log.is_empty() {
            let mut log = div()
                .id("load-log")
                .absolute()
                .top_2()
                .right_2()
                .p_2()
                .rounded(px(4.))
                .bg(theme::get().overlay)
                .text_size(px(11.))
                .flex()
                .flex_col()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(i) = &mut this.island {
                        i.show_log = false;
                    }
                    cx.notify();
                }));
            for line in &island.load_log {
                log = log.child(line.clone());
            }
            root =
                root.child(log.child(div().text_color(theme::get().muted).child("click to hide")));
        }
        let viewport = root
            // A Library card dropped here is placed where it lands, and the
            // Place tool keeps holding it for more clicks.
            .on_drop(cx.listener(move |this, drag: &LibraryDrag, window, cx| {
                this.drop_library(view, drag, window, cx)
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && let Some(i) = &mut this.island {
                    i.pointer_left();
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e, window, cx| this.island_mouse_down(view, e, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e, window, cx| this.island_mouse_down(view, e, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, e, window, cx| this.island_mouse_down(view, e, window, cx)),
            )
            .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, window, cx| {
                if let Some(i) = &mut this.island {
                    let _span = gpui_editor_kit::profile::span("island mouse move");
                    // Hovering and camera drags only change the viewports.
                    // Anything else (a released drag finishing, brushes, gizmos)
                    // may change other panes.
                    let camera_only = match &i.views[view].drag {
                        None => e.pressed_button.is_none(),
                        Some(Drag::Orbit(_) | Drag::Pan(..)) => e.pressed_button.is_some(),
                        Some(_) => false,
                    };
                    i.mouse_move(view, e);
                    if camera_only {
                        this.camera_moved(view, window, cx);
                    } else {
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.island_mouse_up(view, cx)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.island_mouse_up(view, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| this.island_mouse_up(view, cx)),
            )
            .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                let Some(i) = &mut this.island else { return };
                let delta = match event.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                    ScrollDelta::Lines(p) => p.y * 32.,
                };
                i.zoom(view, delta, event.position);
                this.camera_moved(view, window, cx);
            }));
        viewport
            // Centred, so the gutters match whatever room is left over.
            .child(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .right_2()
                    .flex()
                    .justify_center()
                    .child(tools),
            )
            .into_any_element()
    }

    fn island_mouse_down(
        &mut self,
        view: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keys (tools, [ ], Escape) must reach the editor after a viewport click.
        self.focus.focus(window, cx);
        if let Some(i) = &mut self.island {
            i.mouse_down(view, event);
            self.drain_messages();
            cx.notify();
        }
    }

    /// Places a Library card dropped on viewport `view` on the ground
    /// under the cursor, arming the Place tool with it as a click does.
    fn drop_library(
        &mut self,
        view: usize,
        drag: &LibraryDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &drag.pick {
            // Placed even under Scatter: a drop is a placement, where a
            // click there collects into the scatter set.
            Pick::Prop(path) => {
                if let Some(i) = &mut self.island {
                    i.choose_prop(path);
                }
            }
            Pick::Object(island, guid, config) => {
                self.choose_catalog_object(island, *guid, config.clone())
            }
            Pick::Group(_) => return,
        }
        let cursor = window.mouse_position();
        if let Some(i) = &mut self.island {
            match i.ground(view, cursor) {
                Some(at) => i.place_at(at),
                None => i.say("Drop it on the island’s ground", true),
            }
        }
        self.focus.focus(window, cx);
        self.drain_messages();
        cx.notify();
    }

    fn island_mouse_up(&mut self, view: usize, cx: &mut Context<Self>) {
        if let Some(i) = &mut self.island {
            i.finish_drag(view);
            // Drags that end in an edit (harbours, auto placement) report.
            self.drain_messages();
            cx.notify();
        }
    }

    /// The Islands pane: this project's islands, then every shipped one by
    /// province and size. A click opens it.
    pub fn render_island_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.island_list.is_none()
            && !self.island_list_loading
            && let (Some(install), Some(assets)) = (self.install.clone(), self.assets.clone())
        {
            self.island_list_loading = true;
            cx.spawn(async move |this, cx| {
                let list = cx
                    .background_executor()
                    .spawn(async move { islands(&install, Some(&assets)) })
                    .await;
                this.update(cx, |this, cx| {
                    this.island_list = Some(Arc::new(list));
                    this.island_list_loading = false;
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        let words: Vec<String> = (self.island_filter.split_whitespace())
            .map(str::to_ascii_lowercase)
            .collect();
        // The path, plus the names the list shows: "Albion" or "Medium 01"
        // find islands whose files say celtic and medium_01.
        let game = self.game;
        let matches = |path: &str| {
            let region = game.island_region(path).to_ascii_lowercase();
            let p = format!(
                "{path} {} {}",
                island_title(path),
                super::wizard::province(&region)
            )
            .to_ascii_lowercase();
            words.iter().all(|w| p.contains(w.as_str()))
        };
        let open = self.island.as_ref().map(|i| i.path.clone());
        let stem = |path: &str| {
            let name = path.rsplit('/').next().unwrap_or(path);
            name.strip_suffix(".a7m").unwrap_or(name).to_owned()
        };
        // Rows go through the virtualized tree list: only the visible ones
        // are built, and the game ships hundreds. Headers fold their group.
        #[derive(Clone)]
        enum Row {
            Island(String),
            Region(String),
            NewIsland,
            Note,
        }
        let closed = self.island_regions_closed.clone();
        let mut rows: Vec<(Row, tree::Item)> = Vec::new();
        // A folding row: a region at depth 0, a size group inside it at 1.
        let heading_at = |rows: &mut Vec<_>, key: &str, label: String, detail: String, depth| {
            let open = !closed.contains(key);
            rows.push((
                Row::Region(key.to_owned()),
                tree::Item {
                    depth,
                    disclosure: if open {
                        Disclosure::Expanded
                    } else {
                        Disclosure::Collapsed
                    },
                    label: label.into(),
                    detail: detail.into(),
                    selected: false,
                },
            ));
            open
        };
        // Counts in the right column, as the size groups and the
        // Hierarchy show them.
        let header = |rows: &mut Vec<_>, key: &str, label: String, count: usize| {
            heading_at(rows, key, label, count.to_string(), 0)
        };
        let shipped_label = |path: &str| island_title(path);
        let island = |path: &str, detail: String, label: String| {
            (
                Row::Island(path.to_owned()),
                tree::Item {
                    depth: 1,
                    disclosure: Disclosure::Leaf,
                    label: label.into(),
                    detail: detail.into(),
                    selected: open.as_deref() == Some(path),
                },
            )
        };
        // The mod's own islands always come first, even with none yet.
        let own: Vec<_> = (self.project.islands.iter())
            .filter(|i| matches(&i.path))
            .collect();
        // No count while empty: the "New island…" row under it says so.
        let own_count = match own.len() {
            0 => String::new(),
            n => n.to_string(),
        };
        if heading_at(&mut rows, "this-mod", "This mod".into(), own_count, 0) {
            if self.project.islands.is_empty() {
                rows.push((
                    Row::NewIsland,
                    tree::Item {
                        depth: 1,
                        disclosure: Disclosure::Leaf,
                        // A drawn plus, not a typed one: "+ New island…" in
                        // body text read as an island of that name.
                        label: "New island…".into(),
                        detail: SharedString::default(),
                        selected: false,
                    },
                ));
            }
            for i in own {
                rows.push(island(
                    &i.path,
                    // One name per region in compact places: "Albion", as
                    // the viewport chip says it, not the project's "Celtic".
                    format!("{} · {}", super::province_short(&i.region), i.size),
                    stem(&i.path),
                ));
            }
        }
        match self.island_list.clone() {
            None => rows.push((
                Row::Note,
                tree::Item {
                    depth: 0,
                    disclosure: Disclosure::Leaf,
                    label: "Listing the game’s islands…".into(),
                    detail: SharedString::default(),
                    selected: false,
                },
            )),
            Some(all) => {
                let kind = self.island_kind;
                let mut by_region: BTreeMap<&str, Vec<&Skeleton>> = BTreeMap::new();
                for s in all
                    .iter()
                    .filter(|s| matches(&s.path) && kind.admits(&s.path))
                {
                    by_region.entry(&s.region).or_default().push(s);
                }
                // The biggest region first: Anno 117's 22 Albion islands sat
                // above the 59 Latium ones the game is mostly about.
                let mut by_region: Vec<_> = by_region.into_iter().collect();
                by_region.sort_by_key(|(_, islands)| std::cmp::Reverse(islands.len()));
                for (region, mut islands) in by_region {
                    // By size, then by name: the size headers below make the
                    // order readable ("Deco 08" is 64 m, apart from Deco 01).
                    // The path last: base game before a DLC's namesake.
                    islands.sort_by_cached_key(|s| {
                        (s.world[0], island_title(&s.path), s.path.clone())
                    });
                    let shown = header(
                        &mut rows,
                        region,
                        format!("Game · {}", super::wizard::province(region)),
                        islands.len(),
                    );
                    if !shown {
                        continue;
                    }
                    let mut size = None;
                    let mut size_open = true;
                    for s in &islands {
                        // A folding caption per size, so the per-row "128 m"
                        // could go, names line up, and a long size group
                        // can be skipped.
                        if size != Some(s.world[0]) {
                            size = Some(s.world[0]);
                            let n = islands.iter().filter(|o| o.world[0] == s.world[0]).count();
                            size_open = heading_at(
                                &mut rows,
                                &format!("{region}/{}", s.world[0]),
                                // "map": the width, not the island's class, so
                                // a "Small 01" under 256 m does not read wrong.
                                format!("{} m map", s.world[0]),
                                n.to_string(),
                                1,
                            );
                        }
                        if size_open {
                            // A base island whose name a DLC one shares says
                            // so too; an untagged row beside a tagged twin
                            // read as unexplained.
                            let twin = islands.iter().any(|o| {
                                o.path != s.path
                                    && o.world[0] == s.world[0]
                                    && island_title(&o.path) == island_title(&s.path)
                            });
                            rows.push(island(
                                &s.path,
                                island_pack(&s.path)
                                    .or_else(|| twin.then(|| "Base game".to_owned()))
                                    .unwrap_or_default(),
                                shipped_label(&s.path),
                            ));
                        }
                    }
                }
            }
        }
        // Each island's outline from its map image, so islands are told
        // apart by shape, not only by number.
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            let dir = self.config.join("thumbnails");
            self.thumbs = Some(super::catalog::Thumbs::start(install, dir));
        }
        let icons: Vec<Option<Arc<RenderImage>>> = match &mut self.thumbs {
            Some(thumbs) => {
                thumbs.request(rows.iter().filter_map(|(r, _)| match r {
                    Row::Island(path) => Some(path.as_str()),
                    _ => None,
                }));
                rows.iter()
                    .map(|(r, _)| match r {
                        Row::Island(path) => thumbs.get(path),
                        _ => None,
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        self.poll_thumbs(cx);
        let rows = std::rc::Rc::new(rows);
        let paths = rows.clone();
        let heading_rows = rows.clone();
        let glyph_rows = rows.clone();
        let peek_rows = rows.clone();
        let list = tree::List::new(
            "island-list",
            rows.len(),
            move |i| {
                let item = &rows[i].1;
                tree::Item {
                    label: item.label.clone(),
                    detail: item.detail.clone(),
                    ..*item
                }
            },
            move |this: &mut Editor, i, _, cx| match &paths[i].0 {
                Row::Island(path) => this.open_island(path, cx),
                Row::Region(key) => {
                    if !this.island_regions_closed.remove(key) {
                        this.island_regions_closed.insert(key.clone());
                    }
                }
                Row::NewIsland => this.open_wizard(),
                Row::Note => {}
            },
        )
        .icon(move |i| icons.get(i).cloned().flatten())
        // Outlines on the sea blue the wizard's preview uses: on the grey
        // tile, grey rocky coasts all but vanished, and islands are picked
        // here by shape.
        .icon_backdrop(rgb(crate::colors::SEA))
        .glyph(move |i| matches!(glyph_rows[i].0, Row::NewIsland).then_some(Icon::PLUS))
        // Size groups and notes are captions, not islands.
        .heading(move |i| match heading_rows[i].0 {
            Row::Note => true,
            Row::Region(_) => heading_rows[i].1.depth > 0,
            _ => false,
        })
        // Tall enough that outlines can be told apart: islands are chosen
        // by shape as much as by name.
        .row_height(32.)
        // A hovered island previews in the empty Inspector: opening one
        // takes seconds, and twins like "Small 01" and "Small 01 · DLC 1"
        // were told apart from 30 px outlines.
        .decorate({
            let (rows, entity) = (peek_rows, cx.entity().downgrade());
            move |i, row| {
                let Row::Island(path) = &rows[i].0 else {
                    return row;
                };
                let (path, entity) = (path.clone(), entity.clone());
                row.on_hover(move |hovered, _, cx| {
                    let path = path.clone();
                    let _ = entity.update(cx, |this, cx| {
                        this.after_dwell(path.clone(), *hovered, cx, move |this, cx| {
                            this.peek_island(&path, cx)
                        })
                    });
                })
            }
        })
        .highlight(self.island_filter.clone())
        .render(&self.island_list_rows, window, cx);
        // What the island is for, as the list's second filter: decoration
        // and campaign islands outnumber the playable ones.
        let kinds = widgets::segmented(
            "island-kinds",
            IslandKind::ALL.map(|k| (k, k.label().into())),
            &self.island_kind,
            cx,
            |this, kind, _, _| this.island_kind = kind,
        )
        .tooltip(Tooltip::text(
            "Island kind",
            "",
            "Campaign & NPC: campaign, pirate and third-party islands",
        ));
        let search = text_field(
            "island-list-filter",
            &self.island_filter,
            self.island_filter_edit.as_ref().map(|e| e.display()),
            "Search islands…",
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            this.island_filter_edit =
                Some(TextEdit::new((), this.island_filter.clone(), Filter::Text));
            cx.notify();
        }));
        // A filled search gets a way out in one click, as Assets' has.
        let filled = !self.island_filter.is_empty();
        let search = div()
            .relative()
            .m_1()
            .child(search.when(filled, |d| d.pr(px(22.))))
            .when(filled, |d| {
                d.child(
                    clear_search("island-list-filter-clear").on_click(cx.listener(
                        |this, _, _, cx| {
                            cx.stop_propagation();
                            this.island_filter.clear();
                            this.island_filter_edit = None;
                            cx.notify();
                        },
                    )),
                )
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            // A placement waiting for an island, from Assets' Place on an
            // island: said here, where the island is picked, with a way out.
            // Said before it left, the reason was gone by the time the list
            // was up.
            .when_some(
                self.place_pending
                    .as_ref()
                    .map(|cfg| format!("Open an island to place {}", pretty(mesh_name(cfg)))),
                |d, says| {
                    d.child(
                        div()
                            .mx_1()
                            .mt_1()
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(px(3.))
                            .bg(theme::get().selected)
                            .text_size(px(11.))
                            .child(div().flex_1().min_w_0().truncate().child(says))
                            .child(
                                div()
                                    .id("cancel-place-pending")
                                    .flex_shrink_0()
                                    .text_color(theme::get().accent)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.place_pending = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                },
            )
            .child(search)
            .child(div().px_1().pb_1().child(kinds))
            .child(list.flex_1().min_h_0())
            .into_any_element()
    }

    /// Objects grouped by asset and props grouped by mesh file. Object names
    /// come from assets.xml once it has loaded.
    pub fn render_island_hierarchy(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let assets = self.assets.clone();
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            let dir = self.config.join("thumbnails");
            self.thumbs = Some(super::catalog::Thumbs::start(install, dir));
        }
        let Some(island) = &self.island else {
            return crate::editor::pane_note("Open an island to list its objects and props.");
        };
        let filter = island.filter.to_lowercase();
        // "#518" finds instances by the number their rows show; any other
        // text matches model names.
        let by_id = filter.strip_prefix('#');
        // Off by itself once nothing is selected, so the list never empties.
        let selected_only = island.selected_only && !island.selection.is_empty();
        // For the header's toggle, after the island borrow ends.
        let (selection_len, selected_only_on) = (island.selection.len(), island.selected_only);
        let keep = |name: &str, id: u64| match by_id {
            Some(n) => (id & 0xffff_ffff).to_string().starts_with(n),
            None => filter.is_empty() || name.to_lowercase().contains(&filter),
        };
        let chosen = |item: Item| !selected_only || island.selection.contains(&item);
        let mut groups: Groups = BTreeMap::new();
        // Every group's and section's size before the search and the
        // Selected filter: filtered, "Bush 06a marsh 1" read against the
        // Inspector's "68 on this island", so a filtered count says "of".
        let mut totals: BTreeMap<(u8, u32), usize> = BTreeMap::new();
        let mut section_totals = [0usize; 2];
        for o in island.doc.objects.iter() {
            // Keyed by the shown name, so the groups sort as they read.
            let name = pretty(&object_name(assets.as_deref(), o.guid));
            *totals.entry((0, o.guid)).or_default() += 1;
            section_totals[0] += 1;
            if keep(&name, o.id) && chosen(Item::Object(o.id)) {
                groups
                    .entry((0, name, o.guid))
                    .or_default()
                    .push((Item::Object(o.id), o.position));
            }
        }
        for p in island.doc.props.iter() {
            let raw = mesh_name(
                island
                    .doc
                    .meshes
                    .get(p.source.file as usize)
                    .map_or("", |m| m.as_str()),
            );
            // Keyed and searched by the shown name, as objects are: by the
            // file name, "06a" found nothing and "01a" sorted above "01".
            // The file name still matches ("marshveg").
            let name = pretty(raw);
            *totals.entry((1, p.source.file)).or_default() += 1;
            section_totals[1] += 1;
            if (keep(&name, p.id) || keep(raw, p.id)) && chosen(Item::Prop(p.id)) {
                groups
                    .entry((1, name, p.source.file))
                    .or_default()
                    .push((Item::Prop(p.id), p.source.position));
            }
        }
        // Different assets can share a display name (two "Mountain Slot"
        // GUIDs); those groups show their GUID or prop file too.
        // Keyed by the name as shown: two raw names can format alike.
        let mut seen: BTreeMap<(u8, String), u32> = BTreeMap::new();
        for (kind, name, _) in groups.keys() {
            *seen.entry((*kind, pretty(name))).or_default() += 1;
        }
        // "1 of 68" while filtered, the bare count otherwise.
        let filtered = selected_only || !filter.is_empty();
        let of = |shown: usize, total: usize| {
            if filtered && shown < total {
                format!(
                    "{} of {}",
                    crate::project::count(shown),
                    crate::project::count(total)
                )
            } else {
                crate::project::count(shown)
            }
        };
        // Flattened rows: section and group headers, then open instances.
        let mut rows: Vec<HierarchyRow> = Vec::new();
        let mut nth: BTreeMap<(u8, String), u32> = BTreeMap::new();
        let mut tips: BTreeMap<usize, String> = BTreeMap::new();
        // Folded group rows holding part of the selection, drawn lighter
        // than a selected row.
        let mut holding: BTreeSet<usize> = BTreeSet::new();
        let mut section = None;
        // A searched-out section stays as a dim "no matches" row: dropped,
        // a search for "bush" made the Objects section vanish without a
        // trace.
        // "Selected only" keeps it too, so the two filters agree.
        let missed = |kind: u8| {
            ((!filter.is_empty() || selected_only)
                && section_totals[usize::from(kind)] > 0
                && !groups.keys().any(|(k, ..)| *k == kind))
            .then(|| HierarchyRow {
                action: RowAction::NoMatches,
                depth: 0,
                label: if kind == 0 { "Objects" } else { "Props" }.into(),
                detail: if filter.is_empty() {
                    "none selected"
                } else {
                    "no matches"
                }
                .into(),
                disclosure: Disclosure::Leaf,
                selected: false,
            })
        };
        rows.extend(missed(0));
        for ((kind, name, key), items) in &groups {
            // An id search opens what it found, folded sections included.
            let folded =
                by_id.is_none() && !selected_only && island.closed_sections & (1 << kind) != 0;
            if section != Some(*kind) {
                section = Some(*kind);
                rows.push(HierarchyRow {
                    action: RowAction::Section(*kind),
                    depth: 0,
                    label: if *kind == 0 {
                        "Objects".into()
                    } else {
                        "Props".into()
                    },
                    // Instances in the section, as the groups count theirs.
                    detail: of(
                        groups
                            .iter()
                            .filter(|((k, ..), _)| k == kind)
                            .map(|(_, items)| items.len())
                            .sum::<usize>(),
                        section_totals[usize::from(*kind)],
                    ),
                    disclosure: if folded {
                        Disclosure::Collapsed
                    } else {
                        Disclosure::Expanded
                    },
                    selected: false,
                });
            }
            if folded {
                continue;
            }
            let group = if *kind == 0 {
                Group::Object(*key)
            } else {
                Group::Prop(*key)
            };
            // An id search opens the groups it found, or its hits stay hidden.
            let open = by_id.is_some() || selected_only || island.open_groups.is_expanded(group);
            let name = &pretty(name);
            let shared = seen.get(&(*kind, name.clone())).is_some_and(|n| *n > 1);
            // Two assets with one name are numbered; which GUID or file
            // each is shows on hover.
            let number = shared.then(|| {
                let n = nth.entry((*kind, name.clone())).or_insert(0);
                *n += 1;
                *n
            });
            if shared {
                tips.insert(
                    rows.len(),
                    format!("{} {key}", if *kind == 0 { "GUID" } else { "Mesh file" }),
                );
            } else if name.chars().count() > 28 {
                // About what a default-width panel shows before the middle
                // cut; past it the full name is on hover.
                tips.insert(rows.len(), name.clone());
            }
            let held = if open {
                0
            } else {
                items
                    .iter()
                    .filter(|(i, _)| island.selection.contains(i))
                    .count()
            };
            let count = of(
                items.len(),
                totals.get(&(*kind, *key)).copied().unwrap_or(0),
            );
            if held > 0 {
                holding.insert(rows.len());
            }
            rows.push(HierarchyRow {
                action: RowAction::Group(group),
                depth: 1,
                label: match number {
                    Some(n) => format!("{name} {n}"),
                    None => name.clone(),
                },
                // Said in words: a highlighted folded group with a bare
                // "549" read as all 549 selected. "sel.", not "selected":
                // the long form cut the name to "Bush generic…marsh",
                // hiding the "01a" that tells it from its sibling.
                detail: if held > 0 {
                    format!("{} sel. · {count}", crate::project::count(held))
                } else {
                    count
                },
                disclosure: if open {
                    Disclosure::Expanded
                } else {
                    Disclosure::Collapsed
                },
                // A group is "selected" only as the Place tool's model; a
                // selected instance highlights its own row, or its folded
                // group's lighter `holding` mark.
                selected: island.tool == Tool::Place
                    && matches!(group, Group::Prop(f) if island.place == Some(f)),
            });
            if open {
                let (by, desc) = island.instance_sort;
                let arrow = |k: u8| match (by == k, desc) {
                    (false, _) => "",
                    (true, false) => " ▴",
                    (true, true) => " ▾",
                };
                // The selection gestures once, on the caption: on every row
                // the tooltip covered the next rows' coordinates while the
                // pointer moved down the list to box instances.
                tips.insert(
                    rows.len(),
                    "Rows: Ctrl-click adds or removes · Shift-click selects a range · Double-click frames"
                        .into(),
                );
                // The axes named once over the group's instances, as columns,
                // not on every row: 68 rows of "x y z" read as noise.
                rows.push(HierarchyRow {
                    action: RowAction::Caption,
                    // Under the group's name, as its children.
                    depth: 3,
                    // Short enough not to be cut beside three columns; the
                    // active sort marked as the Assets list marks its own.
                    label: format!("ID{}", arrow(0)),
                    // Tab-separated: the kit draws aligned columns.
                    detail: format!("X (m){}\tY (m){}\tZ (m){}", arrow(1), arrow(2), arrow(3)),
                    disclosure: Disclosure::Leaf,
                    selected: false,
                });
                // Ordered as the caption's sort says; a few thousand at most.
                let mut ordered = items.to_vec();
                match island.instance_sort.0 {
                    0 => ordered.sort_by_key(|(item, _)| match *item {
                        Item::Prop(id) | Item::Object(id) => id & 0xffff_ffff,
                    }),
                    k => {
                        let k = usize::from(k - 1);
                        ordered.sort_by(|a, b| a.1[k].total_cmp(&b.1[k]));
                    }
                }
                if island.instance_sort.1 {
                    ordered.reverse();
                }
                for (item, [x, y, z]) in ordered {
                    let id = match item {
                        Item::Prop(id) | Item::Object(id) => id,
                    };
                    rows.push(HierarchyRow {
                        action: RowAction::Item(item),
                        depth: 3,
                        // The group row names the model; instances are
                        // told apart by number and position.
                        label: format!("#{}", id & 0xffff_ffff),
                        // One decimal, as the inspector shows positions,
                        // in the caption's X Y Z columns.
                        detail: format!("{x:.1}\t{y:.1}\t{z:.1}"),
                        disclosure: Disclosure::Leaf,
                        selected: island.selection.contains(&item),
                    });
                }
            }
        }
        rows.extend(missed(1));
        // Each instance row's group row, kept at the top with the caption
        // under it while the instances scroll: 549 bare #ids said nothing
        // about which model they were.
        // With the section above it ("Props 3,853") pinned too, as the
        // Assets tree pins a branch: after a viewport pick it had scrolled
        // away, and with it the place and the one-click way back.
        let owners: Vec<(Option<usize>, Option<usize>)> = rows
            .iter()
            .enumerate()
            .scan((None, None), |(section, group), (i, r)| {
                Some(match r.action {
                    RowAction::Section(_) => {
                        *section = Some(i);
                        *group = None;
                        (None, None)
                    }
                    RowAction::NoMatches => {
                        *section = None;
                        *group = None;
                        (None, None)
                    }
                    RowAction::Group(_) => {
                        *group = Some(i);
                        (*section, None)
                    }
                    RowAction::Caption | RowAction::Item(_) => (*section, *group),
                })
            })
            .collect();
        let click_rows = rows.clone();
        // Group rows carry an eye; hidden groups keep theirs lit.
        let eyes: Vec<Option<(Group, bool)>> = rows
            .iter()
            .map(|r| match r.action {
                RowAction::Group(g) => Some((g, !island.hidden_groups.contains(&g))),
                _ => None,
            })
            .collect();
        // A viewport pick's row, scrolled into view once (its group was
        // opened at the pick).
        if let Some(item) = island.reveal.take()
            && let Some(row) = rows
                .iter()
                .position(|r| matches!(r.action, RowAction::Item(i) if i == item))
        {
            island.open_groups.scroll_to(row);
        }
        // The Objects and Props headers carry the Layers switches.
        let headers: Vec<Option<(bool, bool)>> = rows
            .iter()
            .map(|r| match r.action {
                RowAction::Section(0) => Some((true, island.layers.objects)),
                RowAction::Section(_) => Some((false, island.layers.props)),
                _ => None,
            })
            .collect();
        // Prop groups show their model's Library render: thirty identical
        // cubes told no bush from a rock. The cube stands in until the
        // render lands.
        // Keyed as the Library keys them, with forward slashes; the island's
        // mesh list keeps the game's backslashes.
        let cfgs: Vec<Option<String>> = rows
            .iter()
            .map(|r| match r.action {
                RowAction::Group(Group::Prop(file)) => island
                    .doc
                    .meshes
                    .get(file as usize)
                    .map(|m| m.replace('\\', "/")),
                _ => None,
            })
            .collect();
        let pictures: Vec<Option<Arc<RenderImage>>> = match &mut self.thumbs {
            Some(thumbs) => {
                thumbs.request(cfgs.iter().flatten().map(String::as_str));
                // Row size: the shrunk copy keeps thin stems.
                cfgs.iter()
                    .map(|c| {
                        c.as_deref()
                            .and_then(|c| thumbs.get_small(c).or_else(|| thumbs.get(c)))
                    })
                    .collect()
            }
            None => vec![None; rows.len()],
        };
        let has_picture: Vec<bool> = pictures.iter().map(Option::is_some).collect();
        let entity = cx.entity().downgrade();
        let glyph_rows = rows.clone();
        let caption_rows = rows.clone();
        let hover_rows = rows.clone();
        let list = tree::List::new(
            "island-hierarchy",
            rows.len(),
            move |i| {
                let row = &rows[i];
                tree::Item {
                    depth: row.depth,
                    disclosure: row.disclosure,
                    label: row.label.clone().into(),
                    detail: row.detail.clone().into(),
                    selected: row.selected,
                }
            },
            move |this: &mut Editor, i, event, cx| {
                let Some(isl) = &mut this.island else { return };
                match click_rows[i].action {
                    // The caption sorts: its ID by the row, its X Y Z by
                    // their columns (on_cell_click below).
                    RowAction::Caption => {
                        let (by, desc) = isl.instance_sort;
                        isl.instance_sort = (0, by == 0 && !desc);
                    }
                    RowAction::Section(kind) => isl.closed_sections ^= 1 << kind,
                    RowAction::NoMatches => {}
                    RowAction::Group(group) => {
                        // A prop group click also picks it for Place.
                        if let Group::Prop(file) = group {
                            isl.place = Some(file);
                        }
                        isl.open_groups.toggle(group);
                    }
                    RowAction::Item(item) => {
                        isl.tool = Tool::Select;
                        let m = event.modifiers();
                        let toggle = m.secondary(cx);
                        // Ctrl (Cmd) toggles one instance, Shift takes the rows from
                        // the last plain click, as file lists do.
                        let anchor = isl.anchor.and_then(|a| {
                            click_rows
                                .iter()
                                .position(|r| matches!(r.action, RowAction::Item(x) if x == a))
                        });
                        match (m.shift, anchor) {
                            (true, Some(from)) => {
                                let (lo, hi) = (from.min(i), from.max(i));
                                if !toggle {
                                    isl.selection.clear();
                                }
                                isl.selection
                                    .extend(click_rows[lo..=hi].iter().filter_map(
                                        |r| match r.action {
                                            RowAction::Item(x) => Some(x),
                                            _ => None,
                                        },
                                    ));
                            }
                            _ if toggle => {
                                if !isl.selection.remove(&item) {
                                    isl.selection.insert(item);
                                }
                                isl.anchor = Some(item);
                            }
                            _ => {
                                isl.selection = BTreeSet::from([item]);
                                isl.anchor = Some(item);
                            }
                        }
                        isl.scene_stale = true;
                        if event.click_count() == 2 && !m.shift && !toggle {
                            isl.frame();
                        }
                    }
                }
            },
        )
        // A kind glyph on group rows, so the outliner scans by shape as
        // well as by name: water, coast, slots and props read apart.
        .icon(move |i| pictures[i].clone())
        .glyph(move |i| match glyph_rows[i].action {
            RowAction::Group(Group::Prop(_)) if has_picture[i] => None,
            RowAction::Group(Group::Prop(_)) => Some(Icon::CUBE),
            RowAction::Group(Group::Object(_)) => {
                let name = glyph_rows[i].label.to_lowercase();
                Some(if name.contains("coast") {
                    COAST_ICON
                } else if name.contains("lake") {
                    LAKE_ICON
                } else if name.contains("river") {
                    RIVER_GLYPH
                } else if name.contains("slot") {
                    SLOT_GLYPH
                } else if name.contains("cliff") || name.contains("mountain") {
                    CLIFF_ICON
                } else if name.contains("model") {
                    Icon::CUBE
                } else {
                    MARKER_ICON
                })
            }
            _ => None,
        })
        .decorate(move |i, row| {
            let row = match tips.get(&i) {
                Some(tip) => row.tooltip(Tooltip::text("", "", tip.clone())),
                None => row,
            };
            // A folded group over the selection (a search folds them all):
            // the accent edge of a selected row over a faint fill, so it
            // says "in here", not "all of this is selected".
            let row = if holding.contains(&i) {
                row.bg(theme::get().selected.alpha(0.375)).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.))
                        .bg(theme::get().accent),
                )
            } else {
                row
            };
            if let Some((objects, shown)) = headers[i] {
                let entity = entity.clone();
                return row.group("hierarchy-row").child(
                    widgets::visibility_toggle(("hierarchy-layer", i), shown, "hierarchy-row")
                        .tooltip(Tooltip::text(
                            if shown { "Hide all" } else { "Show all" },
                            "",
                            "The same switch as Layers → Objects / Props",
                        ))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = entity.update(cx, |this: &mut Editor, cx| {
                                if let Some(isl) = &mut this.island {
                                    let layer = if objects {
                                        &mut isl.layers.objects
                                    } else {
                                        &mut isl.layers.props
                                    };
                                    *layer = !*layer;
                                    isl.scene_stale = true;
                                }
                                cx.notify();
                            });
                        }),
                );
            }
            let Some((group, shown)) = eyes[i] else {
                // Same width as an eye, so every count lines up.
                let row = row.child(div().w(px(22.)).flex_shrink_0());
                let RowAction::Item(item) = hover_rows[i].action else {
                    return row;
                };
                let entity = entity.clone();
                return row.on_hover(move |hovered, _, cx| {
                    let _ = entity.update(cx, |this: &mut Editor, cx| {
                        if let Some(isl) = &mut this.island {
                            let to = if *hovered {
                                Some(item)
                            } else {
                                isl.hover_item.filter(|&h| h != item)
                            };
                            isl.set_hover_item(to);
                            cx.notify();
                        }
                    });
                });
            };
            let entity = entity.clone();
            row.group("hierarchy-row").child(
                widgets::visibility_toggle(("hierarchy-eye", i), shown, "hierarchy-row")
                    .tooltip(Tooltip::text(
                        if shown {
                            "Hide in the viewport"
                        } else {
                            "Show in the viewport"
                        },
                        "",
                        "Hidden groups are not drawn or picked; saving keeps them",
                    ))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        let _ = entity.update(cx, |this: &mut Editor, cx| {
                            if let Some(isl) = &mut this.island {
                                if !isl.hidden_groups.remove(&group) {
                                    isl.hidden_groups.insert(group);
                                    // A hidden item cannot stay selected.
                                    let hidden: Vec<_> = (isl.selection.iter().copied())
                                        .filter(|it| isl.item_group(*it) == Some(group))
                                        .collect();
                                    for it in hidden {
                                        isl.selection.remove(&it);
                                    }
                                }
                                isl.scene_stale = true;
                            }
                            cx.notify();
                        });
                    }),
            )
        })
        // The instance columns' caption in the small heading style.
        // Captions, and searched-out sections drawn as quietly so the eye
        // lands on the section with results.
        .heading(move |i| {
            matches!(
                caption_rows[i].action,
                RowAction::Caption | RowAction::NoMatches
            )
        })
        .on_cell_click(|this: &mut Editor, _, col, _| {
            if let Some(isl) = &mut this.island {
                let key = col as u8 + 1;
                let (by, desc) = isl.instance_sort;
                isl.instance_sort = (key, by == key && !desc);
            }
        })
        // Banded, as the asset list: a coordinate far right of its #ID
        // stays on its row.
        .banded()
        // The section, the group row, then its caption one row below it.
        .sticky(move |i| match owners.get(i).copied() {
            Some((section, Some(group))) => section.into_iter().chain([group, group + 1]).collect(),
            Some((Some(section), None)) => vec![section],
            _ => Vec::new(),
        })
        // Room for the model renders: at 20px a bush was a dark speck.
        .row_height(26.)
        // A "#518" search matches ids, which the detail column shows as is.
        .highlight(if island.filter.starts_with('#') {
            String::new()
        } else {
            island.filter.clone()
        })
        .render(&island.open_groups, window, cx)
        .flex_1()
        .min_h_0();
        let search = text_field(
            "hierarchy-filter",
            &island.filter,
            island.filter_edit.as_ref().map(|e| e.display()),
            // Short enough to show whole beside the toggles.
            "Search name or #ID…",
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            if let Some(i) = &mut this.island {
                i.filter_edit = Some(TextEdit::new((), i.filter.clone(), Filter::Text));
            }
            cx.notify();
        }));
        let filled = !island.filter.is_empty();
        let search = div()
            .relative()
            .m_1()
            .child(search.when(filled, |d| d.pr(px(22.))))
            .when(filled, |d| {
                d.child(clear_search("hierarchy-filter-clear").on_click(cx.listener(
                    |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(i) = &mut this.island {
                            i.set_filter(String::new());
                            i.filter_edit = None;
                        }
                        cx.notify();
                    },
                )))
            });
        let any_open = island.open_groups.any_expanded();
        // After this frame's requests, so the renders asked for above are
        // waited on even when no other pane polls.
        self.poll_thumbs(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(search.flex_1())
                    // The selection alone, its groups open: after a box pick
                    // or Select all, the working set without the other
                    // thousands of rows. Always there, framed as a toggle:
                    // appearing on the first click it narrowed the search
                    // field under the pointer, and unframed it read as a
                    // caption.
                    // A checkbox, as the other "only" filters (Library's "Only
                    // on this island", Assets' "Only files in several
                    // archives"): framed like the search field beside it, it
                    // read as a second text box. Faint with nothing selected.
                    .child({
                        let (on, n) = ((selected_only_on && selection_len > 0), selection_len);
                        // The count in the label, house style ("Models · 25"):
                        // in a right-hand slot "Selected only 1" read as one
                        // name. None at 0, where the toggle is faint anyway.
                        let label = match n {
                            0 => "Selected only".to_owned(),
                            n => format!("Selected only · {}", crate::project::count(n)),
                        };
                        widgets::checkbox(label, on, "")
                            .flex_shrink_0()
                            .mr_1()
                            .when(n == 0, |d| d.opacity(0.5))
                            .tooltip(Tooltip::text(
                                if on {
                                    "Show everything"
                                } else {
                                    "Show only the selection"
                                },
                                "",
                                if n == 0 {
                                    "Select something first"
                                } else {
                                    "Lists the selected objects and props with their groups open"
                                },
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(i) = &mut this.island
                                    && !i.selection.is_empty()
                                {
                                    i.selected_only = !i.selected_only;
                                }
                                cx.notify();
                            }))
                    })
                    // One click back from hundreds of open instances to the
                    // groups; the selection stays.
                    .child(
                        div()
                            .id("hierarchy-collapse-all")
                            .h(px(24.))
                            .px_1()
                            .gap_1()
                            .mr_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .when(any_open, |d| {
                                d.cursor_pointer().hover(|s| s.bg(theme::get().hover))
                            })
                            .child(Icon::COLLAPSE_ALL.render(
                                14.,
                                if any_open {
                                    theme::get().text
                                } else {
                                    theme::get().disabled
                                },
                            ))
                            // Icon only, named in its tooltip: with its label
                            // the row cut the search field's placeholder to
                            // "…or #I".
                            .w(px(24.))
                            .tooltip(Tooltip::text("Collapse all groups", "", ""))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(i) = &mut this.island {
                                    i.open_groups.collapse_all();
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    /// What the viewport draws: scene layers, a gameplay-grid overlay and
    /// what the ground shows.
    pub fn render_island_layers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(island) = &self.island else {
            return crate::editor::pane_note("Open an island to choose what the view shows.");
        };
        let layers = island.layers;
        // Debug views fold away and start closed: at the weight of Water and
        // Shadows they read as everyday settings. Made before `toggle`.
        let (diagnostics, diagnostics_open) =
            widgets::collapsible("layers-diagnostics", "Diagnostics", false, window, cx);
        // Made before `toggle`, which holds `cx` for the rest of the list.
        let shading = div()
            .px_2()
            .pt_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(hint("Terrain shading"))
            .child(
                dropdown(
                    "ground-shading",
                    vec![
                        (Shading::Ground, "Materials".into()),
                        (Shading::Map, "Map image".into()),
                        (Shading::Height, "Height".into()),
                        (Shading::Materials, "Material ids".into()),
                    ],
                    &island.shading,
                    window,
                    cx,
                    |this: &mut Editor, s, _, _| {
                        if let Some(i) = &mut this.island {
                            i.shading = s;
                            i.rebuild_tiles(None);
                        }
                    },
                )
                .w_full(),
            );
        // Also made before `toggle`.
        let grid = div()
            .px_2()
            .pt_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(hint("Gameplay grid"))
            .child(
                dropdown(
                    "grid-overlay",
                    std::iter::once(None)
                        .chain(grids::OVERLAYS.map(Some))
                        .map(|kind| (kind, kind.map_or("None", grids::label).into()))
                        .collect(),
                    &layers.grid,
                    window,
                    cx,
                    |this: &mut Editor, kind, _, _| {
                        if let Some(i) = &mut this.island {
                            i.layers.grid = kind;
                        }
                    },
                )
                .w_full(),
            );
        let toggle = |label: &'static str, on: bool, detail: String, f: fn(&mut Layers)| {
            checkbox(label, on, detail)
                .px_2()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(i) = &mut this.island {
                        f(&mut i.layers);
                        i.scene_stale = true;
                    }
                    cx.notify();
                }))
        };
        let shipped_tris = island.shipped.triangles();
        let scene_layers = div()
            .w_full()
            .flex()
            .flex_col()
            .py_2()
            // First in the panel: the tab bar's border is its rule already.
            // The pane's scope, once, above every section: on the Show
            // heading it read as covering Show alone.
            .child(
                div()
                    .px_2()
                    .pb_1()
                    // Muted, not disabled: it is a real distinction.
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("View settings · not saved with the island"),
            )
            .child(div().px_2().child(section("Show")))
            .child(toggle(
                "Objects",
                layers.objects,
                crate::project::count(island.doc.objects.len()),
                |l| l.objects = !l.objects,
            ))
            .child(toggle(
                "Props",
                layers.props,
                crate::project::count(island.doc.props.len()),
                |l| l.props = !l.props,
            ))
            .child(toggle("Water", layers.water, String::new(), |l| {
                l.water = !l.water
            }))
            .children(island.prop_sets.as_ref().map(|_| {
                // "plants": the grass in view is the terrain's texture, and
                // "Ground cover" off beside it read as a wrong state.
                toggle(
                    // The Inspector's word for it, so its note points here.
                    "Ground cover",
                    layers.vegetation,
                    match island.vegetation_count() {
                        Some((n, all)) if all > n => format!("{n} of {all}"),
                        Some((n, _)) => n.to_string(),
                        None => String::new(),
                    },
                    |l| l.vegetation = !l.vegetation,
                )
            }))
            // Off while grass fills the view read as an ignored switch.
            .when(island.prop_sets.is_some() && !layers.vegetation, |d| {
                d.child(
                    // Lined up under the toggle's label: its inset, box and
                    // gap come to 30 px.
                    div()
                        .pl(px(30.))
                        .pr_2()
                        .child(hint("Grass in the terrain texture always shows.")),
                )
            })
            // How it is drawn, apart from what is drawn.
            .child(div().px_2().child(section("Rendering")))
            .child(toggle(
                "Ground tint",
                layers.tint,
                if island.doc.tint.is_some() {
                    String::new()
                } else {
                    "none".into()
                },
                |l| l.tint = !l.tint,
            ))
            .child(toggle("Shadows", layers.shadows, String::new(), |l| {
                l.shadows = !l.shadows
            }))
            // The base look of the ground belongs with how it is drawn.
            .child(shading)
            // Guides drawn over the scene, not what the game shows.
            .child(div().px_2().child(section("Guides")))
            .child(
                // A guide line, not game art. The colour is named in words; a swatch at the row's end
                // read as a stray control.
                toggle("Harbour coasts", layers.harbours, String::new(), |l| {
                    l.harbours = !l.harbours
                })
                .tooltip(Tooltip::text(
                    "Harbour coasts",
                    "",
                    "Cyan lines where ships can dock; the start coast is drawn thicker",
                )),
            )
            .child(toggle("Buildable", layers.buildable, String::new(), |l| {
                l.buildable = !l.buildable
            }))
            // Same name as the Inspector section that lists them.
            .child(toggle(
                "Sound & blocker areas",
                layers.spots,
                String::new(),
                |l| l.spots = !l.spots,
            ))
            .child(grid)
            // The editor's own checks against the game's data, apart from
            // the guides a modder works by.
            .child(div().px_2().pt_1().child(diagnostics))
            .when(diagnostics_open, |d| {
                d.child(toggle(
                    "Game’s original terrain mesh",
                    layers.shipped,
                    if shipped_tris > 0 {
                        format!("{shipped_tris} tris")
                    } else {
                        String::new()
                    },
                    |l| l.shipped = !l.shipped,
                ))
                .child(toggle(
                    "Terrain chunks (32 m)",
                    layers.chunks,
                    String::new(),
                    |l| l.chunks = !l.chunks,
                ))
            });
        let handle = self.scrolls.get("island-layers");
        widgets::vscroll("island-layers", &handle, scene_layers)
            .size_full()
            .bg(theme::get().panel)
            .into_any_element()
    }

    /// The island's material set as clickable thumbnails.
    pub fn render_island_materials(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(island) = &self.island else {
            return crate::editor::pane_note("Open an island to paint with its ground materials.");
        };
        if island.thumbs.is_empty() {
            return empty_pane("This island has no material set or material maps");
        }
        let names = island.palette.as_ref();
        let mut grid = div()
            .id("material-palette")
            .flex()
            .flex_wrap()
            .gap_1()
            .p_1()
            .w_full();
        for (id, image) in &island.thumbs {
            let id = *id;
            let selected = island.paint == id;
            let name = names
                .and_then(|p| p.materials.get(&id))
                .map_or(String::new(), |m| mesh_name(&m.file).to_owned());
            grid = grid.child(
                div()
                    .id(("material", u32::from(id)))
                    .relative()
                    .size(px(THUMB))
                    .rounded(px(3.))
                    .overflow_hidden()
                    .border_2()
                    .border_color(if selected {
                        theme::get().highlight
                    } else {
                        theme::get().border
                    })
                    .cursor_pointer()
                    .child(img(image.clone()).size_full())
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .w_full()
                            .bg(theme::get().overlay)
                            .text_size(px(10.))
                            .text_center()
                            .child(id.to_string()),
                    )
                    .tooltip(Tooltip::text(
                        name,
                        id.to_string(),
                        "Click to paint with this material",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.paint = id;
                            i.tool = Tool::Paint;
                        }
                        cx.notify();
                    })),
            );
        }
        gpui_editor_kit::widgets::vscroll(
            "material-palette-scroll",
            &self.scrolls.get("material-palette"),
            grid,
        )
        .size_full()
        .bg(theme::get().panel)
        .into_any_element()
    }

    /// Previews `path` in the empty Inspector: its map decoded off the UI
    /// thread at preview size, kept until another row is hovered.
    fn peek_island(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.island.is_some() || self.island_peek.as_ref().is_some_and(|(p, _)| p == path) {
            return;
        }
        let first = self.island_peek.is_none();
        self.island_peek = Some((path.to_owned(), None));
        self.notify_pane(crate::editor::Pane::IslandInspector, cx);
        // The empty viewport's New island steps down once a preview shows.
        if first {
            self.notify_viewports(cx);
        }
        let (Some(install), path) = (self.install.clone(), path.to_owned()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let image = cx
                .background_spawn({
                    let path = path.clone();
                    // 2x the 320 px it shows at, at most.
                    async move { super::catalog::map_preview(&install, &path, 640) }
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some((p, slot)) = &mut this.island_peek
                    && *p == path
                {
                    *slot = image;
                    this.notify_pane(crate::editor::Pane::IslandInspector, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The hovered island, in the Inspector while none is open.
    fn island_peek_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (path, image) = self.island_peek.as_ref()?;
        let all = self.island_list.as_deref().map_or(&[][..], Vec::as_slice);
        let world = all.iter().find(|s| s.path == *path);
        let mut facts = Vec::new();
        if let Some(s) = world {
            facts.push(super::wizard::province(&s.region));
            facts.push(format!("{} m map", s.world[0]));
        }
        // The list's rule: a base island whose name a DLC one shares says
        // "Base game", so the preview tells the twins apart as the rows do.
        let twin = world.is_some_and(|s| {
            all.iter().any(|o| {
                o.path != s.path
                    && o.world[0] == s.world[0]
                    && island_title(&o.path) == island_title(&s.path)
            })
        });
        if let Some(pack) = island_pack(path).or_else(|| twin.then(|| "Base game".to_owned())) {
            facts.push(pack);
        }
        let title = island_title(path);
        let open = path.clone();
        Some(
            div()
                .p_2()
                .flex()
                .flex_col()
                .gap_1()
                // Says it is a preview: with a map in the Inspector, the
                // viewport's "No island open" read as a contradiction.
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child("Island preview"),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(island_title(path)),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(facts.join(" · ")),
                )
                // The wizard preview's sea blue, as behind the list's
                // outlines; the slot keeps its size while the map decodes.
                // The pane's width, as the loaded island's fields run.
                .child(
                    div()
                        .mt_1()
                        .w_full()
                        .max_w(px(320.))
                        .aspect_square()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(theme::get().border)
                        .overflow_hidden()
                        .bg(rgb(crate::colors::SEA))
                        .when_some(image.clone(), |d, image| d.child(img(image).size_full())),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(path.rsplit('/').next().unwrap_or("").to_owned()),
                )
                // The way in here too: the preview stays while the pointer
                // crosses to it, and the row's click was the only way. The
                // file among its neighbours beside it, as the Assets preview
                // offers Show in tree; after a long file name it ran past
                // the pane's edge.
                .child({
                    let shown = path.clone();
                    div()
                        .mt_1()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_3()
                        .child(
                            widgets::action(format!("Open {title}"), widgets::Tone::Primary)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_island(&open, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("peek-show-in-assets")
                                .text_size(px(11.))
                                .text_color(theme::get().accent)
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .child("Show in Assets")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.show_in_assets(&shown, cx);
                                    cx.notify();
                                })),
                        )
                })
                .into_any_element(),
        )
    }

    /// Tool properties and the selected prop.
    pub fn render_island_inspector(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The ground cover presets' main plants, for the picker's pictures;
        // queued while the section can show them.
        if let (Some(island), Some(thumbs)) = (&self.island, &mut self.thumbs)
            && let Some(sets) = &island.prop_sets
        {
            thumbs.request(
                sets.choices(&island.region())
                    .into_iter()
                    .filter_map(|n| sets.get(n))
                    .flat_map(|s| s.main_plants(PRESET_PLANTS)),
            );
        }
        let Some(island) = &self.island else {
            return self.island_peek_panel(cx).unwrap_or_else(|| {
                crate::editor::pane_note("Point at an island in the list to preview it here.")
            });
        };
        let mut body = div()
            .id("island-inspector")
            .flex()
            .flex_col()
            .p_2()
            .gap_1()
            .w_full();
        let assets = self.assets.clone();
        // A stale id must not take the inspector down; show the first live
        // item. Without a selection the island's own sections are the page.
        let Some(&first) = island
            .selection
            .iter()
            .find(|&&item| island.doc.matrix(item).is_some())
        else {
            // Nothing selected: the page is the island's; the tool's name
            // and key live in the floating panel.
            let (title, rows) = play_checklist(self, island, cx);
            body = body
                .child(island_heading(island))
                .child(section(&title))
                .child(rows);
            // No "click a prop" line here: the floating strip says it, and
            // twice on one screen read as filler.
            return finish_inspector(self, body, window, cx);
        };
        // No "Selection" caption: the item's bold name below heads the page.
        // Header lines and the placement details differ per item kind; the
        // transform fields and buttons are shared.
        let (title, model, details): (String, String, Vec<(&str, String)>) = match first {
            Item::Prop(id) => {
                let Some(p) = island.doc.prop(id) else {
                    return finish_inspector(self, body, window, cx);
                };
                let model = island
                    .doc
                    .meshes
                    .get(p.source.file as usize)
                    .cloned()
                    .unwrap_or_default();
                let s = &p.source;
                (
                    format!("{} #{id}", pretty(mesh_name(&model))),
                    model,
                    vec![
                        ("Kind", "Prop".into()),
                        (
                            "Tint",
                            format!(
                                "{:.2} {:.2} {:.2} {:.2}",
                                s.color[0], s.color[1], s.color[2], s.color[3]
                            ),
                        ),
                        (
                            "Follows terrain",
                            if s.adapt_terrain_height.is_some() {
                                "Yes"
                            } else {
                                "No"
                            }
                            .into(),
                        ),
                    ],
                )
            }
            Item::Object(id) => {
                let Some(o) = island.doc.objects.iter().find(|o| o.id == id) else {
                    return finish_inspector(self, body, window, cx);
                };
                let asset = assets.as_deref().and_then(|a| a.get(o.guid));
                let model = asset
                    .and_then(|a| o.model(&a.cfgs))
                    .or(o.config.as_deref())
                    .unwrap_or("")
                    .to_owned();
                (
                    format!(
                        "{} #{}",
                        object_name(assets.as_deref(), o.guid),
                        id & 0xffff_ffff
                    ),
                    model,
                    vec![
                        ("Kind", "Object".into()),
                        ("GUID", o.guid.to_string()),
                        ("Template", asset.map_or("?".into(), |a| a.template.clone())),
                        ("Section", o.section.name().into()),
                        ("Area", format!("{:#06x}", o.area)),
                        ("ID", o.id.to_string()),
                        ("Variation", o.variation.to_string()),
                        ("Owner", o.owner.to_string()),
                    ],
                )
            }
        };
        let (name, number) = title.rsplit_once(" #").unwrap_or((title.as_str(), ""));
        body = body
            // The name as the heading, and the kind, instance number and
            // model file on one muted line (whole on hover).
            .child({
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    // Up to two lines before a cut: the name is what
                    // confirms the pick, and the instance number, already
                    // in the Hierarchy and the status bar, moved to the
                    // file line so it never takes the name's room.
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .line_clamp(2)
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name.to_owned()),
                    )
                    // On the selection's own heading: under the island's
                    // caption it read as deselecting the island. Esc does
                    // the same, which nothing else on screen says.
                    .child(
                        div()
                            .id("inspector-deselect")
                            .ml_auto()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child("Deselect")
                            .tooltip(Tooltip::text(
                                "Deselect",
                                "Esc",
                                "Back to the island’s checklist and settings",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(i) = &mut this.island {
                                    i.selection.clear();
                                    i.push_overlay();
                                }
                                cx.notify();
                            })),
                    )
            })
            .when(island.selection.len() > 1, |b| {
                b.child(info_row("Selected", island.selection.len().to_string()))
            })
            .child(
                // The model line opens its file in Assets: the way from a
                // placed prop to its textures and LODs.
                {
                    let fact = |name: &str| {
                        details
                            .iter()
                            .find(|(l, _)| *l == name)
                            .map(|(_, v)| v.clone())
                    };
                    let kind = fact("Kind").map(|kind| match number {
                        "" => kind,
                        n => format!("{kind} #{n}"),
                    });
                    let file = model.rsplit('/').next().unwrap_or(&model).to_owned();
                    // Facts that cannot be edited ride on the file line, not
                    // in rows that look like fields. The file is a link to
                    // its Assets entry, said as one: muted, it hid that.
                    div()
                        .flex()
                        .gap_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .children(kind.map(|k| {
                            if model.is_empty() {
                                k
                            } else {
                                format!("{k} ·")
                            }
                        }))
                        // Water has no model file: the kind and number alone.
                        .when(!model.is_empty(), |d| {
                            let model = model.clone();
                            d.child(
                                div()
                                    .id("selection-model")
                                    .min_w_0()
                                    // Cut in the middle, as tree rows are: the
                                    // variant and ".cfg" at the end tell
                                    // props apart, and long names ran to the
                                    // pane's edge.
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .text_color(theme::get().accent)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .tooltip(Tooltip::text("Show in Assets", "", model.clone()))
                                    .child(file)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.show_in_assets(&model, cx);
                                        cx.notify();
                                    })),
                            )
                        })
                },
            );
        // One prop of many alike: how many, and a way to take them all, as
        // retinting or removing every marsh bush starts there.
        if let (Item::Prop(id), 1) = (first, island.selection.len())
            && let Some(file) = island.doc.prop(id).map(|p| p.source.file)
        {
            let alike = island
                .doc
                .props
                .iter()
                .filter(|p| p.source.file == file)
                .count();
            if alike > 1 {
                // The model's Library render beside the count, so a tiny pick
                // is confirmed by its look; on the name line it cut the name.
                let picture = self.thumbs.as_ref().and_then(|t| {
                    let cfg = model.replace('\\', "/");
                    // The Library tile's render: the 1/3-size copy, scaled
                    // up to 54 px, was a dark smudge beside the same model's
                    // bright tile.
                    t.get(&cfg).or_else(|| t.get_small(&cfg))
                });
                // One line, the count said once: "1 selected · 68
                // instances" over "Select all 68 of this model" said 68
                // twice in the Inspector's densest block.
                // The count on its own line, the two actions under it, beside
                // the picture: on one line a three-digit count pushed "Place
                // more" past the pane's edge.
                let actions = div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .id("select-alike")
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            // Its reach in the label: a bare "all" read as
                            // the whole island and "same" left the reach
                            // to guess; the count under "68 placed" says it.
                            .child(format!("Select all {}", crate::project::count(alike)))
                            .tooltip(Tooltip::text(
                                "Select every placement",
                                "",
                                format!("Selects all {alike} placements of this model"),
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(i) = &mut this.island {
                                    i.selection = i
                                        .doc
                                        .props
                                        .iter()
                                        .filter(|p| p.source.file == file)
                                        .map(|p| Item::Prop(p.id))
                                        .collect();
                                    i.push_overlay();
                                }
                                cx.notify();
                            })),
                    )
                    // Another of these is the usual next step: Place with
                    // this model armed, as a Library card click arms it.
                    .child(div().text_color(theme::get().muted).child("·"))
                    .child(
                        div()
                            .id("place-more")
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child("Place more")
                            .tooltip(Tooltip::text(
                                "Place more",
                                "",
                                "Switches to Place with this model ready",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(i) = &mut this.island {
                                    i.place = Some(file);
                                    i.place_object = None;
                                    i.tool = Tool::Place;
                                }
                                cx.notify();
                            })),
                    );
                let lines = div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(theme::get().muted)
                            // A count, not a position: "1 of 549" beside
                            // #517, the third row, read as a place in the list.
                            .child(format!("{} on this island", crate::project::count(alike))),
                    )
                    // Its size as placed (the model's bounds at its scale):
                    // a tiny bush under a box many times its size otherwise
                    // needed explaining.
                    .when_some(
                        self.thumbs
                            .as_ref()
                            .and_then(|t| t.look(&model.replace('\\', "/")))
                            .map(|look| {
                                let k = island.field_value(Field::Scale);
                                let [w, h, d] = look.size.map(|v| v * k);
                                // The mesh's box at this scale, with the scale
                                // it includes, said in the tooltip: the Scale
                                // field two rows down shows it already.
                                (
                                    format!("Size {w:.1} × {h:.1} × {d:.1} m"),
                                    format!(
                                        "Width × height × depth of the model's bounds, as placed at scale {k:.2}×"
                                    ),
                                )
                            }),
                        |d, (label, tip)| {
                            d.child(
                                div()
                                    .id("prop-size")
                                    .text_color(theme::get().muted)
                                    .child(label)
                                    .tooltip(Tooltip::text("Size", "", tip)),
                            )
                        },
                    )
                    .child(actions);
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .when_some(picture, |d, picture| {
                            d.child(
                                // As tall as the two lines beside it: at 44 px
                                // a sparse bush was a smudge.
                                div()
                                    .size(px(64.))
                                    .flex_shrink_0()
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(theme::get().border)
                                    .overflow_hidden()
                                    .bg(theme::get().picture_bg)
                                    .child(img(picture).size_full()),
                            )
                        })
                        .child(lines),
                );
            }
        }
        // How it sits on the ground is the Y field's tooltip only: it is
        // what a move does to Y, and as a line under the fields ("Keeps
        // its height when moved ⓘ") it read as a setting that won't toggle.
        let ground = details
            .iter()
            .find(|(l, _)| *l == "Follows terrain")
            .map(|(_, v)| v == "Yes");
        // Units inside every field, as rotation's ° and scale's ×.
        // One six-column grid: X, Y and Z take two columns each, rotation
        // and scale three, so both rows share their outer edges and the
        // second row's halves are equal.
        let mut grid = div().grid().grid_cols(6).gap_1();
        for (axis, label) in ["X", "Y", "Z"].into_iter().enumerate() {
            let field = self
                .island_compact(Field::Position(axis), label, "m", cx)
                .col_span(2)
                .min_w_0();
            grid = grid.child(match (axis, ground) {
                (1, Some(follows)) => field.tooltip(Tooltip::text(
                    "Height",
                    "",
                    if follows {
                        "Horizontal moves follow the terrain"
                    } else {
                        "Horizontal moves keep this height"
                    },
                )),
                _ => field,
            });
        }
        grid = grid
            .child(
                self.island_compact(Field::Yaw, "Rotation Y", "°", cx)
                    .col_span(3)
                    .min_w_0(),
            )
            // Half the row, so the "×" has room again.
            .child(
                self.island_compact(Field::Scale, "Scale", "×", cx)
                    .col_span(3)
                    .min_w_0(),
            );
        // "Position" over X/Y/Z: beside the Size line above, bare numbers
        // read as another size.
        body = body
            .child(section("Transform"))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .mb(px(2.))
                    .child("Position"),
            )
            .child(grid);
        // Rivers and lakes are curves: their edit lives in the River tool.
        if let Item::Object(id) = first
            && island
                .doc
                .objects
                .iter()
                .find(|o| o.id == id)
                .and_then(|o| island.water_kind(o.guid))
                .is_some()
        {
            // A framed button: unframed it read as a caption. The
            // double-click is its tooltip.
            body = body.child(
                widgets::action("Edit curve", widgets::Tone::Secondary)
                    .w_full()
                    .tooltip(Tooltip::text(
                        "Edit curve",
                        "",
                        "Or double-click the water in the view",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.edit_water(id);
                        }
                        cx.notify();
                    })),
            );
        }
        let details: Vec<_> = details
            .into_iter()
            .filter(|(l, _)| !matches!(*l, "Kind" | "Follows terrain"))
            .collect();
        // The engine's own fields under a fold: at GUID's weight they
        // read as settings, and pushed the actions down.
        let (details, engine): (Vec<_>, Vec<_>) = details
            .into_iter()
            .partition(|(l, _)| !matches!(*l, "Section" | "Area" | "ID" | "Variation" | "Owner"));
        // A heading over a single row overstates it.
        if details.len() > 1 {
            body = body.child(section("Placement"));
        }
        for (label, value) in details {
            // A tint is a colour: show it, with the numbers after.
            let tint: Option<Vec<f32>> = (label == "Tint")
                .then(|| value.split(' ').filter_map(|v| v.parse().ok()).collect())
                .filter(|c: &Vec<f32>| c.len() >= 3);
            let white = (tint.as_ref()).is_some_and(|c| c.iter().all(|v| (v - 1.).abs() < 1e-3));
            // No tint, no row: a "None" in a value column nothing else
            // used read as a control.
            if white {
                continue;
            }
            body = body.child(match tint {
                Some(c) => {
                    let byte = |v: f32| (v.clamp(0., 1.) * 255.) as u32;
                    let color = byte(c[0]) << 16 | byte(c[1]) << 8 | byte(c[2]);
                    // Label above, as "Position (m)" is: the swatch lines
                    // up with the fields instead of floating mid-row.
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        // The swatch shows the stored tint; nothing here
                        // edits it, and under editable fields it looked
                        // like a colour picker.
                        .child(hint(format!("{label} · read-only")))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(widgets::swatch(color))
                                .child(hint(if c.iter().all(|v| (v - 1.).abs() < 1e-3) {
                                    // White multiplies by one: the model as authored.
                                    "White, no tint".to_owned()
                                } else {
                                    format!(
                                        "#{color:06x} · alpha {:.2}",
                                        c.get(3).copied().unwrap_or(1.)
                                    )
                                })),
                        )
                        .into_any_element()
                }
                None => info_row(label, value).into_any_element(),
            });
        }
        if !engine.is_empty() {
            let (heading, open) =
                widgets::collapsible("inspector-game-data", "Game data", false, window, cx);
            body = body.child(heading);
            if open {
                for (label, value) in engine {
                    body = body.child(info_row(label, value));
                }
            }
        }
        // Properties first, then the commands on the selection as the
        // section's last row; Move/Rotate/Scale live in the Select tool's bar.
        body = body.child(
            div()
                .flex()
                .gap_2()
                .pt_2()
                .child(
                    widgets::action("Duplicate", widgets::Tone::Secondary)
                        .flex_1()
                        .tooltip(Tooltip::text(
                            "Duplicate",
                            "Ctrl D",
                            "Copies the selection in place",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(i) = &mut this.island {
                                i.duplicate_selected();
                            }
                            cx.notify();
                        })),
                )
                .child(
                    // A neutral button with red text: a red frame at rest made
                    // Delete the loudest thing in the Inspector. Undo covers it.
                    widgets::action("Delete", widgets::Tone::Secondary)
                        .text_color(theme::get().error)
                        .flex_1()
                        .tooltip(Tooltip::text(
                            "Delete",
                            "Delete",
                            "Removes the selection; Ctrl Z undoes",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(i) = &mut this.island {
                                i.delete_selected();
                            }
                            cx.notify();
                        })),
                ),
        );
        // A game original keeps its way into the mod while a prop is
        // selected: a button of its own under the selection's commands, as
        // the checklist has with nothing selected. A link on the folded
        // settings heading read as part of that section.
        // Past a rule and under the island's name, so it reads as the
        // island's command, not the prop's; the resting button's shape and
        // tone, one command in one look.
        if !self.project.islands.iter().any(|i| i.path == island.path) {
            body = body
                .child(div().mt_3().h(px(1.)).bg(theme::get().border))
                .child(
                    div()
                        .pt_2()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(format!("Island · {}", island_title(&island.path))),
                );
            body = body.child(
                widgets::action(
                    format!("Add island to ‘{}’", self.project.info.name),
                    // Secondary under a selection: the prop's own controls lead
                    // while it is being edited (filled, it outranked them).
                    widgets::Tone::Secondary,
                )
                .mt_1()
                .self_start()
                .gap_2()
                .child(widgets::keycap("Ctrl S"))
                .tooltip(Tooltip::text(
                    "Add to the mod",
                    "Ctrl S",
                    "Copies this game island into the mod, where the game loads it from",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(i) = &mut this.island {
                        i.want_in_mod();
                    }
                    this.save();
                    cx.notify();
                })),
            );
        }
        finish_inspector(self, body, window, cx)
    }
}

/// The Inspector's heading for the island itself: its name in the selection
/// title's style, and what it is on a muted line, as a prop's file shows
/// under its name.
/// What the status bar says while something is selected: the first item's
/// name as the Inspector titles it, and how many more. None with nothing
/// selected, so the last action's message shows instead.
pub(crate) fn selection_status(island: &IslandEditor, assets: Option<&Assets>) -> Option<String> {
    let first = island.selection.iter().next()?;
    let name = match *first {
        Item::Prop(id) => {
            let p = island.doc.prop(id)?;
            let mesh = island.doc.meshes.get(p.source.file as usize)?;
            format!("{} #{id}", pretty(mesh_name(mesh)))
        }
        // The Inspector's title ("Lake #1"), not a bare "Object #1".
        Item::Object(id) => {
            let guid = island
                .doc
                .objects
                .iter()
                .find(|o| o.id == id)
                .map(|o| o.guid)?;
            format!("{} #{}", object_name(assets, guid), id & 0xffff_ffff)
        }
    };
    Some(match island.selection.len() {
        1 => format!("Selected {name}"),
        n => format!("{n} selected · {name} and {} more", n - 1),
    })
}

/// Whether the island is ready to play: a harbour coast to settle from, a
/// place on the test map, and the island saved into the mod. "Saved" means
/// in the mod: a game original untouched is not there yet. Returns the
/// heading, with its count, and the rows; each unmet row carries its action.
fn play_checklist(
    editor: &Editor,
    island: &IslandEditor,
    cx: &mut Context<Editor>,
) -> (String, Div) {
    let coasts = island.coast_guids();
    let harbours = island
        .doc
        .objects
        .iter()
        .filter(|o| coasts.contains(&o.guid))
        .count();
    let ours = editor.project.islands.iter().any(|i| i.path == island.path);
    let saved = ours && !island.dirty();
    let on_test_map = island.listing.on_test_map;
    // A game original's place on the test map is the game's, not yet the
    // mod's: done only once the island is the mod's own. A green tick before
    // the copy read as progress the mod had not made.
    let mod_test_map = on_test_map && ours;
    // A game original already on the test map takes that place with it
    // when added: not a step of its own, so it is not counted, and "1 of
    // 3" beside a row saying it would come along read as missing work.
    let follows = on_test_map && !ours;
    let steps: &[bool] = if follows {
        &[harbours > 0, saved]
    } else {
        &[harbours > 0, mod_test_map, saved]
    };
    let (done, total) = (steps.iter().filter(|d| **d).count(), steps.len());
    // A row: the state in plain text, and for an unmet one the
    // action as the only link, so it reads as a state and a button.
    let check = |id: &'static str,
                 done: bool,
                 label: &'static str,
                 state: String,
                 action: Option<(&'static str, CheckAction)>| {
        // Not done and nothing to do here: it follows from another
        // step (a game original's test map place, from adding it).
        // Grey, not amber: amber is for what needs an action.
        // ("In the mod" has no link either, but the button under the
        // list is its action, so it stays amber.)
        let follows = !done && action.is_none() && id == "ready-test-map";
        // Not a step of its own (and not counted): no circle, said as a
        // note under the step it follows. With a circle, "1 of 2" over
        // three marked rows read as a miscount.
        if follows {
            return div()
                .flex()
                .items_center()
                .gap_2()
                .pl(px(20.))
                .text_size(px(11.))
                .text_color(theme::get().muted)
                // Plain words under its step: the "↳" read as a code
                // mark in an otherwise worded checklist.
                .child(format!("{label}: {}", state.to_lowercase()));
        }
        div()
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(11.))
            .child(
                div()
                    .w(px(12.))
                    .text_color(if done {
                        theme::get().success
                    } else if follows {
                        theme::get().muted
                    } else {
                        theme::get().highlight
                    })
                    .child(if done { "✓" } else { "○" }),
            )
            .child(div().w(px(70.)).text_color(theme::get().muted).child(label))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when(follows, |d| d.text_color(theme::get().muted))
                    .child(state),
            )
            .when_some(action, |d, (word, act)| {
                d.child(
                    div()
                        .id(id)
                        .flex_shrink_0()
                        .text_color(theme::get().accent)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(word)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            act(this, cx);
                            cx.notify();
                        })),
                )
            })
    };
    // "Ready" only once it is: until then a checklist.
    let title = if done == total {
        "Ready to play".to_owned()
    } else {
        format!("Play checklist · {done} of {total} done")
    };
    let rows = div()
        .flex()
        .flex_col()
        .gap_1()
        .pb_1()
        .child(check(
            "ready-harbour",
            harbours > 0,
            "Harbour",
            if harbours > 0 {
                crate::project::plural(harbours, "harbour coast", "harbour coasts")
            } else {
                "None yet ·".to_owned()
            },
            (harbours == 0).then(|| {
                (
                    "Harbour tool",
                    Box::new(|this: &mut Editor, _: &mut Context<Editor>| {
                        if let Some(i) = &mut this.island {
                            i.tool = Tool::Harbour;
                        }
                    }) as Box<dyn Fn(&mut Editor, &mut Context<Editor>)>,
                )
            }),
        ))
        // In the order they depend on each other: the test map listing
        // is set once the island is in the mod.
        .child(check(
            "ready-saved",
            saved,
            "In the mod",
            if saved {
                format!("Saved in ‘{}’", editor.project.info.name)
            } else if ours {
                "Unsaved changes ·".to_owned()
            } else {
                // The step is the button under the list.
                // "Not yet": the label in front says "In the mod".
                "Not yet".to_owned()
            },
            // Unsaved edits of the mod's own island: a link, as other
            // rows have. A game original's copy is the button below.
            (ours && !saved).then(|| {
                (
                    "Save",
                    Box::new(|this: &mut Editor, _: &mut Context<Editor>| {
                        if let Some(i) = &mut this.island {
                            i.want_in_mod();
                        }
                        this.save()
                    }) as Box<dyn Fn(&mut Editor, &mut Context<Editor>)>,
                )
            }),
        ))
        // A game original's test map place follows from adding it: said
        // under the Add button as its result, not as a third line under a
        // "1 of 2" checklist, where it read as an uncounted item.
        .when(ours || !on_test_map, |d| {
            d.child(check(
                "ready-test-map",
                mod_test_map,
                "Test map",
                // A game original's place is the game's own listing until
                // it is copied into the mod; "On the test map" beside "In the
                // mod: Not yet" read as done before it was.
                if on_test_map && !ours {
                    "Included when added".to_owned()
                } else if on_test_map {
                    "On the test map".to_owned()
                } else {
                    "Off ·".to_owned()
                },
                (!on_test_map).then(|| {
                    (
                        "Add to test map",
                        Box::new(|this: &mut Editor, _: &mut Context<Editor>| {
                            this.edit_listing(ListingOf::Island, |l| l.on_test_map = true);
                        })
                            as Box<dyn Fn(&mut Editor, &mut Context<Editor>)>,
                    )
                }),
            ))
        })
        // Copying a game original into the mod is where it becomes the
        // modder's to edit: a button, not a small link at a row's end. The
        // viewport chip's words, for the same Ctrl S.
        .when(!ours, |d| {
            d.child(
                // Primary: the step that makes the island the modder's; its
                // key on its face, not only in the tooltip.
                widgets::action(
                    // "Add to", not "Save a copy": "copy" already names the
                    // mod installed in the game, beside it in the top bar.
                    // The band's words, for the same command.
                    format!("Add island to ‘{}’", editor.project.info.name),
                    widgets::Tone::Primary,
                )
                .mt_1()
                // Its own width, as under a selection: one shape for one
                // command, and full width it was the loudest thing on screen.
                .self_start()
                .gap_2()
                .child(widgets::keycap_on_fill("Ctrl S"))
                .tooltip(Tooltip::text(
                    "Add to the mod",
                    "Ctrl S",
                    "Copies this game island into the mod, where the game loads it from",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(i) = &mut this.island {
                        i.want_in_mod();
                    }
                    this.save();
                    cx.notify();
                })),
            )
            .when(on_test_map, |d| {
                d.child(hint("Adding it also puts it on the test map."))
            })
        });
    (title, rows)
}

fn island_heading(island: &IslandEditor) -> Div {
    let region = wizard::province(
        island
            .path
            .split("/provinces/")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or_default(),
    );
    let size = island.doc.field.grid.size()[0];
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .truncate()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(island_title(&island.path)),
        )
        .child(
            div()
                .truncate()
                .text_size(px(11.))
                .text_color(theme::get().muted)
                .child(format!("{region} · {size:.0} m map")),
        )
}

/// Island facts under the inspector, and the pane frame.
fn finish_inspector(
    editor: &Editor,
    mut body: Stateful<Div>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> AnyElement {
    let Some(island) = &editor.island else {
        return body.into_any_element();
    };
    let g = &island.doc.field.grid;
    // Under a selection, the island's sections get a quiet caption of
    // their own: a second bold title read as a second selected object.
    let selected = island
        .selection
        .iter()
        .any(|&item| island.doc.matrix(item).is_some());
    if selected {
        // A band across the pane, as property panels head a category:
        // in the section headings' style alone it read as one more group
        // of the selection, so File & stats looked like the bush's. A bold
        // title read as a second selected object.
        // One fold for all of it, closed by default: under a selection the
        // prop is the task, and the island's checklist, stats, sound and
        // cover stacked below its Transform read as noise mid-edit. Open,
        // each section keeps its own fold.
        let (band, open) = widgets::collapsible(
            "inspector-island-settings",
            "Island settings",
            false,
            window,
            cx,
        );
        // Folded, the band says where the island stands, as folded sections
        // say their values: closed, it hid whether the island was in the mod.
        let in_mod = editor.project.islands.iter().any(|i| i.path == island.path);
        // One fact, short enough to sit beside the name in a narrow pane:
        // a game original's is that it is not in the mod yet (its button
        // is the viewport chip's while this is folded), the mod's own the
        // checklist count.
        // Where the island stands, at the band's end. Not its name: the
        // viewport chip says it, and name and state together ran past a
        // narrow pane's edge.
        let state = if in_mod {
            let (title, _) = play_checklist(editor, island, cx);
            title
                .split_once(" · ")
                .map_or_else(|| "Ready to play".to_owned(), |(_, c)| c.to_owned())
        } else {
            String::new()
        };
        // A fold like the Inspector's others, a rule and a heading, with
        // 16 px of room above to set it apart from the selection: as a
        // filled full-width band it read as a status bar.
        body = body.child(
            band.mt_4()
                // Clear of the scrollbar's edge for the action at its end.
                .pr(px(8.))
                .child({
                    // The checklist's own marks, so the folded band reads as
                    // a status, not a caption: amber ○ for a step still
                    // open, ✓ once ready.
                    let ready = state == "Ready to play";
                    div()
                        .ml_auto()
                        .pl_2()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(theme::get().muted)
                        // The mark only beside a state: before a link it read
                        // as a radio button.
                        .when(in_mod, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(if ready {
                                        theme::get().success
                                    } else {
                                        theme::get().highlight
                                    })
                                    .child(if ready { "✓" } else { "○" }),
                            )
                            .child(div().min_w_0().truncate().child(state))
                        })
                    // Not in the mod, the Add button sits under the
                    // selection's commands, not on this heading.
                }),
        );
        if !open {
            return body.into_any_element();
        }
        // The checklist stays, folded with its count on the heading:
        // selecting a prop must not lose the guide to finishing the island.
        let (title, rows) = play_checklist(editor, island, cx);
        // Folded, the count goes under the heading as the sections below
        // say their values; open, the ticks say it.
        let (name, count) = match title.split_once(" · ") {
            Some((name, count)) => (name.to_owned(), count.to_owned()),
            None => (title, String::new()),
        };
        let (heading, open) = widgets::collapsible("inspector-checklist", name, false, window, cx);
        // Under the caption, whose rule is above it, the first section
        // draws none of its own.
        body = body.child(fold_summary(heading.border_t_0(), open, count));
        if open {
            body = body.child(rows);
        }
        // While a game original is not in the mod, the way in stays under
        // its checklist, folded or not (open, the checklist shows it): with
        // a prop selected the checklist is folded, and on the band it cut
        // the island's name to two letters.
        if !open && !editor.project.islands.iter().any(|i| i.path == island.path) {
            // The checklist's button, compact: one command keeps one form
            // whatever is selected; as a link it read as navigation.
            // Secondary under a selection: the selection's own controls
            // above are what the pane is for then, and a filled button
            // below pulled the eye off Transform.
            body = body.child(
                widgets::action(
                    format!("Add island to ‘{}’", editor.project.info.name),
                    if island.selection.is_empty() {
                        widgets::Tone::Primary
                    } else {
                        widgets::Tone::Secondary
                    },
                )
                .mt_1()
                .mb_1()
                // Its own width: full, it outshouted the selection's own
                // Duplicate and Delete above it. Their height, though: a
                // shorter button two sections down read as a different kind.
                .self_start()
                .gap_2()
                .child(if island.selection.is_empty() {
                    widgets::keycap_on_fill("Ctrl S")
                } else {
                    widgets::keycap("Ctrl S")
                })
                .tooltip(Tooltip::text(
                    "Add to the mod",
                    "Ctrl S",
                    "Copies this game island into the mod, where the game loads it from",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(i) = &mut this.island {
                        i.want_in_mod();
                    }
                    this.save();
                    cx.notify();
                })),
            );
        }
    }
    // Folded either way: open, it pushed the editable presets below read-
    // only numbers; the checklist above covers what an idle pane needs.
    let (heading, open) =
        widgets::collapsible("inspector-island", "File & stats", false, window, cx);
    // A folded section says its value on its heading, as the checklist
    // says its count: folded under a selection the island read as blank.
    body = body.child(fold_summary(
        heading,
        open,
        format!(
            "{:.0} m · {} props",
            g.size()[0],
            crate::project::count(island.doc.props.len())
        ),
    ));
    if open {
        body = body
            // Cut in the middle: "…medium_01" against "…medium_02" is
            // where island files differ; the whole path on hover.
            .child(widgets::info_row_with(
                "File",
                div()
                    .id("inspector-file")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_middle()
                    .tooltip(Tooltip::text("File", "", island.path.clone()))
                    .child(
                        island
                            .path
                            .rsplit('/')
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                    ),
            ))
            .child(info_row(
                "Size",
                format!("{:.0} m · {}² samples", g.size()[0], g.width),
            ))
            // Grouped digits, as the status bar and Layers write them.
            .child(info_row(
                "Triangles",
                crate::project::count(island.triangles()),
            ))
            .child(info_row(
                "Objects",
                crate::project::count(island.doc.objects.len()),
            ))
            .child(info_row(
                "Props",
                crate::project::count(island.doc.props.len()),
            ));
    }
    let marks = island.spot_marks();
    let blockers = marks.iter().filter(|m| m.1 > 0.).count();
    let (heading, open) = widgets::collapsible(
        "inspector-spots",
        "Sound & blocker areas",
        false,
        window,
        cx,
    );
    // What was counted, as the open section's first row says it; a bare
    // "13" needed opening to read. Zero kinds are left out.
    let spots = marks.len() - blockers;
    let summary = [
        (spots > 0).then(|| crate::project::plural(spots, "sound spot", "sound spots")),
        (blockers > 0).then(|| crate::project::plural(blockers, "blocker", "blockers")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    body = body.child(fold_summary(heading, open, summary));
    if open {
        body = body
        .child(info_row(
            "Placed",
            format!(
                "{}, {}",
                crate::project::plural(marks.len() - blockers, "sound spot", "sound spots"),
                crate::project::plural(blockers, "blocker", "blockers")
            ),
        ))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    widgets::action("Auto sound spots", widgets::Tone::Secondary)
                        .flex_1()
                        .tooltip(Tooltip::text(
                            "Auto sound spots",
                            "",
                            "Waterfall sounds at every waterfall, cliff sounds along steep coast. Replaces the ones placed here before; one undo step",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.auto_spots(false);
                            cx.notify();
                        })),
                )
                .child(
                    widgets::action("Auto build blockers", widgets::Tone::Secondary)
                        .flex_1()
                        .tooltip(Tooltip::text(
                            "Auto build blockers",
                            "",
                            "Blocker cubes under rocks and decoration on buildable land, so players cannot build into them. Replaces the ones placed here before; one undo step",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.auto_spots(true);
                            cx.notify();
                        })),
                ),
        );
    }
    body = body.child(session_controls(island, editor.thumbs.as_ref(), window, cx));
    if let Some((id, kind)) = island.selected_slot() {
        let resource = island
            .listing
            .slot_resources
            .get(&id)
            .zip(island.assets.as_ref())
            .map_or("random (map generator)".to_owned(), |(&g, a)| {
                fertility::label(a, g)
            });
        body = body
            .child(section("Slot"))
            .child(info_row("Kind", kind.slot_type()))
            .child(info_row("Test-map resource", resource));
    }
    if island.is_new() {
        body = body
            // The wizard's name for the same settings.
            .child(section("Game placement"))
            .child(editor.listing_controls(ListingOf::Island, cx));
        for (text, warn) in island.harbour_report() {
            body = body.child(
                div()
                    .text_size(px(11.))
                    .text_color(if warn {
                        theme::get().error
                    } else {
                        theme::get().muted
                    })
                    .child(text),
            );
        }
    }
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme::get().panel)
        .child(
            gpui_editor_kit::widgets::vscroll(
                "island-inspector-scroll",
                &editor.scrolls.get("island-inspector"),
                body,
            )
            .flex_1()
            .min_h_0(),
        )
        .into_any_element()
}

/// The island's session settings: the vegetation prop set the game
/// scatters ground props from, and the ambient preset.
fn session_controls(
    island: &super::IslandEditor,
    thumbs: Option<&super::catalog::Thumbs>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Div {
    let session = &island.doc.session;
    // Open while nothing is selected; folded under a selection, where the
    // selection's own fields matter. A key per case keeps both remembered.
    let (heading, open) = widgets::collapsible(
        if island.selection.is_empty() {
            "inspector-settings"
        } else {
            "inspector-settings-selected"
        },
        "Ground cover & lighting",
        island.selection.is_empty(),
        window,
        cx,
    );
    // Made before `pick`, which holds `cx` for the rest of the section.
    let show_cover = cx.listener(|this, _, _, cx| {
        if let Some(i) = &mut this.island {
            i.layers.vegetation = !i.layers.vegetation;
            i.scene_stale = true;
        }
        cx.notify();
    });
    let mut pick = |id: &'static str,
                    mut names: Vec<String>,
                    current: &str,
                    detail: &dyn Fn(&str) -> Option<SharedString>,
                    pictures: &dyn Fn(&str) -> Vec<Arc<RenderImage>>,
                    note: Option<SharedString>,
                    set: fn(&mut SessionSettings, String)| {
        // A value outside the region's list (a one-off on a shipped island)
        // still shows as selected.
        // Developer test sets are not for modders, unless one is in use.
        names.retain(|n| !n.contains("_test") || n == current);
        if !current.is_empty() && !names.iter().any(|n| n == current) {
            names.insert(0, current.to_owned());
        }
        // Shown as words; the two special values say what they do.
        let options = names
            .into_iter()
            .map(|n| {
                let label = match n.as_str() {
                    "Default" => "Game default".to_owned(),
                    // Its detail line says "No plants"; the label need not.
                    "None" => "None".to_owned(),
                    other => preset_name(other),
                };
                let detail = detail(&n);
                let pictures = pictures(&n);
                (n, label.into(), detail, pictures)
            })
            .collect();
        widgets::dropdown_pictured(
            id,
            options,
            note,
            &current.to_owned(),
            window,
            cx,
            move |this: &mut Editor, name, _, _| {
                if let Some(i) = &mut this.island {
                    let grows = name != "None";
                    i.edit_session(|s| set(s, name));
                    // Picking ground cover should change what is on screen:
                    // its preview comes on with the choice. Not for None: it
                    // grows nothing to show.
                    if id == "session-prop-set" && !i.layers.vegetation && grows {
                        i.layers.vegetation = true;
                        i.scene_stale = true;
                    }
                }
            },
        )
    };
    // The preview switch is an eye on the heading, as the Hierarchy's rows
    // carry theirs: as the section's first row, view state sat above the
    // island's saved presets. Quiet while shown; while hidden it says so in
    // words, "Cover hidden", as the eye alone looked like it hid lighting
    // too. Grey either way: amber is for what the modder must act on, and
    // hidden is a fine way to work. Picking a preset turns it back on;
    // Layers → Ground cover is the same switch.
    let shown = island.layers.vegetation;
    let summary = match &island.prop_sets {
        Some(_) => format!(
            "{} · {}",
            preset_name(&session.vegetation_prop_set),
            preset_name(&session.global_ambient)
        ),
        None => preset_name(&session.global_ambient),
    };
    let heading = if island.prop_sets.is_some() {
        heading.child(
            div()
                .id("ground-cover-eye")
                .ml_auto()
                .h(px(20.))
                .px_1()
                .flex()
                .items_center()
                .gap_1()
                .rounded(px(3.))
                .cursor_pointer()
                .font_weight(FontWeight::NORMAL)
                .text_color(theme::get().muted)
                .when(shown, |d| d.opacity(0.35))
                .hover(|s| s.opacity(1.).bg(theme::get().hover))
                // Hidden, it is the way to show it, framed as a small
                // button: as a state and a 14 px eye its fix was a guess.
                .when(!shown, |d| {
                    d.mr_1()
                        .border_1()
                        .border_color(theme::get().border)
                        .bg(theme::get().secondary)
                        .text_color(theme::get().text)
                        // The crossed eye while hidden, as the Hierarchy's
                        // eyes say it: an open eye read as "cover on".
                        .child(Icon::EYE_OFF.render(13., theme::get().muted))
                        // "Show" folded too: the crossed eye alone read as
                        // a state with no way out. "Cover": beside "Ground
                        // cover & lighting", a bare "Show" read as lighting
                        // too.
                        .child("Show cover")
                })
                .when(shown, |d| d.child(Icon::EYE.render(13., theme::get().muted)))
                .tooltip(Tooltip::text(
                    if shown { "Hide ground cover" } else { "Ground cover is hidden" },
                    "",
                    "In the viewport only; the same switch as Layers → Ground cover. Choosing a preset also shows it",
                ))
                .on_click(move |e, w, cx| {
                    cx.stop_propagation();
                    show_cover(e, w, cx);
                }),
        )
    } else {
        heading
    };
    // Against Layers, whose switches are view-only, these save: said on
    // the preset's label, where the saved values start.
    let mut out = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(fold_summary(heading, open, summary));
    if !open {
        return out;
    }
    if let Some(sets) = &island.prop_sets {
        let names = sets
            .choices(&island.region())
            .into_iter()
            .map(str::to_owned)
            .collect();
        // Label above, the list the inspector's full width: preset names
        // differ at their ends, which a narrow field cut off.
        // The preview's switch on the label's row: one quiet control instead
        // of a sentence that flagged the default as wrong.
        out = out.child(hint("Ground cover preset")).child(
            pick(
                "session-prop-set",
                names,
                &session.vegetation_prop_set,
                // What each preset holds, on its option: "some" and
                // "many flowers" were told apart only by picking each.
                // How many plants, and how much of what grows is flowers:
                // "some" and "many flowers" hold the same flower models
                // at different odds.
                &|name: &str| {
                    let set = sets.get(name)?;
                    let props = || {
                        (set.tiers.iter())
                            .flat_map(|t| &t.props)
                            .filter(|p| p.probability > 0. && !p.vegetate)
                    };
                    let mut files: Vec<&str> = props().map(|p| p.file.as_str()).collect();
                    files.sort_unstable();
                    files.dedup();
                    let odds: f32 = props().map(|p| p.probability).sum();
                    let flowers: f32 = props()
                        .filter(|p| p.file.contains("flower"))
                        .map(|p| p.probability)
                        .sum();
                    // "Types": distinct models, not placed plants. Every
                    // row in the same terms, so 0% reads as none, not as
                    // unknown.
                    let plants = crate::project::plural(files.len(), "plant type", "plant types");
                    Some(match files.len() {
                        0 => "No plants".into(),
                        _ if flowers <= 0. => format!("{plants} · no flowers").into(),
                        _ => format!("{plants} · {:.0}% flowers", flowers / odds * 100.).into(),
                    })
                },
                // Its main plants, pictured: "Wetlands 01" and "02" differ
                // by what grows, which counts alone did not show.
                &|name: &str| {
                    let (Some(set), Some(thumbs)) = (sets.get(name), thumbs) else {
                        return Vec::new();
                    };
                    // Only pictures that show something: a 10 cm flower's
                    // tile came out a blank grey square, read as a failed
                    // load. The next most common plant takes its place.
                    set.main_plants(PRESET_PLANTS)
                        .into_iter()
                        .filter(|cfg| thumbs.look(cfg).is_some_and(|l| l.mask.count_ones() >= 6))
                        .filter_map(|cfg| thumbs.get_small(cfg).or_else(|| thumbs.get(cfg)))
                        .take(3)
                        .collect()
                },
                // Picking turns the preview on: said where the pick is. Not "hidden"
                // again: the line under the field says it.
                (!shown).then(|| "Picking a preset with plants turns cover on.".into()),
                |s, n| s.vegetation_prop_set = n,
            )
            .w_full(),
        );
    }
    let set = island
        .prop_sets
        .as_ref()
        .and_then(|p| p.get(&session.vegetation_prop_set));
    if let (Some(set), Some(palette)) = (set, &island.palette) {
        let mut grows: Vec<&str> = palette
            .ground_layers
            .iter()
            .map(|(_, g)| g.as_str())
            .filter(|g| set.grows_on(g))
            .collect();
        grows.sort_unstable();
        grows.dedup();
        // One line under the picker; the long story is in its tooltip.
        let mut tip = "Bushes, grass tufts and flowers the game scatters at run time over \
                       materials by ground type and around trees. Layers → Ground cover \
                       previews them."
            .to_owned();
        if !set.grass.is_empty() {
            let mut grass = set.grass.clone();
            grass.dedup();
            tip += &format!(
                " GPU grass blades on {} are not previewed.",
                grass.join(", ")
            );
        }
        // "Hidden" is the heading's Show cover button alone: said again
        // under the field it was the same state twice.
        out = out.child(
            hint(if grows.is_empty() {
                "No material of this island grows ground cover".to_owned()
            } else {
                format!(
                    "Grows on {}",
                    grows
                        .iter()
                        .map(|g| ground_name(g))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            })
            .id("prop-set-help")
            .tooltip(Tooltip::text("Ground cover preset", "", tip)),
        );
    }
    out = out.child(hint("Lighting & weather").mt_2()).child(
        pick(
            "session-ambient",
            island.ambients.clone(),
            &session.global_ambient,
            &|_: &str| None,
            &|_: &str| Vec::new(),
            None,
            |s, n| s.global_ambient = n,
        )
        .w_full(),
    );
    for (edge, name) in [
        ("North", &session.north_ambient),
        ("South", &session.south_ambient),
    ] {
        if let Some(name) = name {
            out = out.child(info_row(edge, name.clone()));
        }
    }
    // "Saved with the island" once, for both, closing the section in the
    // caption size: on the preset alone the lighting read as view-only, as
    // Layers' settings are; at the top, as a hint, it read as a field label.
    out.child(
        div()
            .pt_1()
            .text_size(px(10.))
            .text_color(theme::get().muted)
            .child("Preset and lighting are saved with the island."),
    )
}

impl IslandEditor {
    /// Overlays drawn on top of the GPU frame: brush ring, chunk grid,
    /// gizmo and marquee.
    fn paint_view(&self, view: usize, bounds: Bounds<Pixels>, window: &mut Window) {
        let _span = gpui_editor_kit::profile::span("paint view");
        let v = &self.views[view];
        let extent = self.extent();
        v.viewport.paint(v.camera, extent, bounds, window);
        let project = |x: f32, z: f32| {
            let y = self.doc.field.height([x, z]);
            v.camera.project([x, y + 0.05, z], bounds, extent)
        };
        {
            let _span = gpui_editor_kit::profile::span("paint view: rivers");
            self.paint_rivers(view, bounds, window);
        }
        if self.overlay() == Some(anno_formats::grids::Kind::WaterFlow) {
            self.paint_flow_arrows(view, bounds, window);
        }
        if self.tool == Tool::Harbour {
            // Point handles; the picked one larger and highlighted.
            for (id, i, p) in self.coast_handles(view) {
                let picked = self.coast_point == Some((id, i));
                let r = if picked { 5. } else { 3. };
                let (x, y) = (f32::from(p.x), f32::from(p.y));
                let square: Vec<_> = [[-r, -r], [r, -r], [r, r], [-r, r], [-r, -r]]
                    .iter()
                    .map(|[dx, dy]| point(px(x + dx), px(y + dy)))
                    .collect();
                let color = if picked {
                    crate::colors::with_alpha(crate::colors::PICK, 0xff)
                } else {
                    crate::colors::with_alpha(crate::colors::HANDLE, 0xe6)
                };
                paint::line(&square, 2., rgba(color), window);
            }
        }
        match &v.drag {
            Some(Drag::Shore {
                preview: Some(curve),
                ..
            }) => {
                let points: Vec<_> = curve
                    .iter()
                    .map(|p| {
                        v.camera
                            .project([p.p[0], p.p[1] + 0.3, p.p[2]], bounds, extent)
                    })
                    .collect();
                paint::line(
                    &points,
                    3.,
                    rgba(crate::colors::with_alpha(crate::colors::PICK, 0xe6)),
                    window,
                );
            }
            Some(Drag::Region { from, to }) => {
                // The rectangle draped over the ground.
                let (x0, x1) = (from[0].min(to[0]), from[0].max(to[0]));
                let (z0, z1) = (from[2].min(to[2]), from[2].max(to[2]));
                let edge = |a: [f32; 2], b: [f32; 2]| -> Vec<Point<Pixels>> {
                    (0..=32)
                        .map(|k| {
                            let t = k as f32 / 32.;
                            project(a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)
                        })
                        .collect()
                };
                for (a, b) in [
                    ([x0, z0], [x1, z0]),
                    ([x1, z0], [x1, z1]),
                    ([x1, z1], [x0, z1]),
                    ([x0, z1], [x0, z0]),
                ] {
                    paint::line(
                        &edge(a, b),
                        2.,
                        rgba(crate::colors::with_alpha(crate::colors::PICK, 0xe6)),
                        window,
                    );
                }
            }
            _ => {}
        }
        let active = match &v.drag {
            Some(Drag::Gizmo { gesture, .. }) => Some(gesture.handle),
            _ => None,
        };
        if let Some(frame) = self.gizmo_frame(view) {
            frame.paint(active, window);
        }
        if let Some(Drag::Marquee(m)) = &v.drag
            && !m.is_click()
        {
            m.paint(window);
        }
    }
}

impl IslandEditor {
    /// Turns view `view` so the map's north (+z) points up the screen,
    /// keeping its tilt and zoom: yaw 0 or a half turn, by the camera's
    /// mirroring.
    fn face_north(&mut self, view: usize) {
        let camera = &mut self.views[view].camera;
        camera.yaw = if camera.mirror {
            0.
        } else {
            std::f32::consts::PI
        };
        self.push_overlay();
    }

    /// North and a scale bar for the Top view, measured from its camera: a
    /// round length that spans 60–160 px at the current zoom.
    fn top_view_scale(&self, b: Bounds<Pixels>) -> Option<Div> {
        let camera = self.views[1].camera;
        let extent = self.extent();
        let c = extent.center;
        let at = |dx: f32, dz: f32| camera.project([c[0] + dx, 0., c[2] + dz], b, extent);
        let o = at(0., 0.);
        let e = at(100., 0.);
        let n = at(0., 100.);
        let per_m = f32::from((e.x - o.x).abs().max((e.y - o.y).abs())) / 100.;
        if !(per_m.is_finite() && per_m > 0.) {
            return None;
        }
        let metres = [1., 2., 5., 10., 20., 50., 100., 200., 500., 1000.]
            .into_iter()
            .find(|m| m * per_m >= 60.)
            .unwrap_or(1000.);
        // Anno's +z is north; pick the arrow nearest to where it points.
        let (nx, ny) = (f32::from(n.x - o.x), f32::from(n.y - o.y));
        let arrow = if ny.abs() >= nx.abs() {
            if ny < 0. { "↑" } else { "↓" }
        } else if nx > 0. {
            "→"
        } else {
            "←"
        };
        Some(
            div()
                .absolute()
                // Above the document pill, clear of the floating tool panel.
                // Stacked over the island badge in the same corner.
                .bottom(px(40.))
                .right_3()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(8.))
                .bg(theme::get().floating)
                .border_1()
                .border_color(theme::get().floating_border)
                .text_size(px(11.))
                .text_color(theme::get().text)
                .child(format!("N {arrow}"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            div()
                                .w(px(metres * per_m))
                                .h(px(5.))
                                .border_1()
                                .border_t_0()
                                .border_color(theme::get().text),
                        )
                        .child(format!("{metres:.0} m")),
                ),
        )
    }

    /// The viewports' world-space guides, drawn by the renderer: chunk
    /// grid, harbour coasts, blockers and sound spots, the brush ring and
    /// selection boxes. Built per event; a few thousand points at most.
    pub(super) fn overlay_lines(&self) -> Vec<gpu::Line> {
        let on_ground = |x: f32, z: f32| [x, self.doc.field.height([x, z]) + 0.05, z];
        let mut lines = Vec::new();
        if self.layers.chunks {
            let [w, d] = self.doc.field.grid.size();
            let step = gpui_editor_kit::heightfield::TILE as f32 * self.doc.field.grid.spacing;
            let mut x = 0.;
            while x <= w {
                let points = (0..=64).map(|i| on_ground(x, d * i as f32 / 64.)).collect();
                lines.push(gpu::Line {
                    points,
                    width: 1.,
                    color: crate::colors::with_alpha(crate::colors::HANDLE, 0x40),
                });
                x += step;
            }
            let mut z = 0.;
            while z <= d {
                let points = (0..=64).map(|i| on_ground(w * i as f32 / 64., z)).collect();
                lines.push(gpu::Line {
                    points,
                    width: 1.,
                    color: crate::colors::with_alpha(crate::colors::HANDLE, 0x40),
                });
                z += step;
            }
        }
        // Harbour curves on the waterline; the start coast thicker.
        let coasts = if self.layers.harbours || self.tool == Tool::Harbour {
            self.coast_guids()
        } else {
            Default::default()
        };
        let start = (self.harbour.as_ref()).and_then(|l| l.guid(harbour::Piece::StartCoast));
        for o in self.doc.objects.iter().filter(|o| coasts.contains(&o.guid)) {
            let Some(path) = &o.path else { continue };
            let points = path
                .iter()
                .map(|p| [p.p[0], p.p[1] + 0.3, p.p[2]])
                .collect();
            // Quieter while something is selected, so the selection leads.
            let quiet = !self.selection.is_empty();
            let width = match (start == Some(o.guid), quiet) {
                (true, false) => 5.,
                (true, true) => 3.,
                (false, false) => 3.,
                (false, true) => 2.,
            };
            lines.push(gpu::Line {
                points,
                width,
                color: crate::colors::with_alpha(
                    crate::colors::COAST,
                    if quiet { 0x80 } else { 0xe6 },
                ),
            });
        }
        if self.layers.spots {
            // Blockers as their square on the ground (red), sound spots as a
            // ring (violet); generated ones solid, others faint.
            for (at, side, generated) in self.spot_marks() {
                let alpha = if generated { 0xe6 } else { 0x80 };
                let points = if side > 0. {
                    let h = side / 2.;
                    [[-h, -h], [h, -h], [h, h], [-h, h], [-h, -h]]
                        .iter()
                        .map(|[dx, dz]| on_ground(at[0] + dx, at[2] + dz))
                        .collect()
                } else {
                    (0..=16)
                        .map(|i| {
                            let a = i as f32 / 16. * std::f32::consts::TAU;
                            on_ground(at[0] + a.cos() * 2., at[2] + a.sin() * 2.)
                        })
                        .collect()
                };
                let color = crate::colors::with_alpha(
                    if side > 0. {
                        crate::colors::AREA_SQUARE
                    } else {
                        crate::colors::AREA_ROUND
                    },
                    0,
                );
                lines.push(gpu::Line {
                    points,
                    width: 1.5,
                    color: color | alpha,
                });
            }
        }
        if self.tool.is_brush()
            && let Some(at) = self.cursor
        {
            // Brush outline draped over the ground.
            let points = (0..=48)
                .map(|i| {
                    let a = i as f32 / 48. * std::f32::consts::TAU;
                    on_ground(
                        at[0] + a.cos() * self.brush.radius,
                        at[2] + a.sin() * self.brush.radius,
                    )
                })
                .collect();
            lines.push(gpu::Line {
                points,
                width: 1.5,
                color: crate::colors::with_alpha(crate::colors::PICK, 0xd0),
            });
        }
        // Selection: a bright ring on the ground under the object marks the
        // thing itself (a sparse bush is lost inside its box), and the
        // model's bounding box, quieter, gives its extent. Both follow
        // live edits.
        // Selection: the model's own outline (outline.wgsl) marks the thing,
        // and the box's corners its extent: brackets, not the whole box, and
        // no ground ring. A full box, ring and gizmo around a 0.6 m bush
        // framed the grass and buried the bush. Each corner carries a fifth
        // of its three edges, orange over a dark stroke so dry grass does
        // not swallow it.
        for corners in self.selection_boxes() {
            // Brackets on the four ground corners only: the outline marks
            // the model itself, and eight brackets plus outline, gizmo and
            // id tag crowded a 1 m bush. The whole box faintly behind them
            // carries the extent: brackets alone on noisy grass read as
            // loose dashes, not one box.
            for i in 0..8 {
                for a in 0..3 {
                    if i & (1 << a) == 0 {
                        let points = vec![corners[i], corners[i | (1 << a)]];
                        // Over a faint dark stroke, as the brackets are: bare,
                        // the edges sank into ochre grass and the box read as
                        // loose brackets. Light, or the edges read brown
                        // beside the orange brackets.
                        lines.push(gpu::Line {
                            points: points.clone(),
                            width: 2.,
                            color: crate::colors::with_alpha(crate::colors::CASING, 0x30),
                        });
                        lines.push(gpu::Line {
                            points,
                            width: 1.,
                            color: crate::colors::with_alpha(crate::colors::SELECTION, 0x90),
                        });
                    }
                }
            }
            for i in bracket_corners(true) {
                for a in 0..3 {
                    let j = i ^ (1 << a);
                    let tip: [f32; 3] = std::array::from_fn(|k| {
                        corners[i][k] + (corners[j][k] - corners[i][k]) * 0.2
                    });
                    let points = vec![corners[i], tip];
                    lines.push(gpu::Line {
                        points: points.clone(),
                        width: 3.,
                        color: crate::colors::with_alpha(crate::colors::CASING, 0xcc),
                    });
                    // Saturated orange: the pale amber matched dry grass.
                    lines.push(gpu::Line {
                        points,
                        width: 1.5,
                        color: crate::colors::with_alpha(crate::colors::SELECTION, 0xff),
                    });
                }
            }
        }
        // The Hierarchy's hovered instance: a white box, apart from the
        // orange selection, so pointing down the list shows which bush a
        // row is without selecting it.
        if let Some(corners) = self
            .hover_item
            .filter(|i| !self.selection.contains(i))
            .and_then(|i| self.item_box(i))
        {
            // The selection's brackets, fainter and in white: a full box
            // read louder than the selection it is pointing beside.
            for i in bracket_corners(flat_box(&corners)) {
                for a in 0..3 {
                    let j = i ^ (1 << a);
                    let tip: [f32; 3] = std::array::from_fn(|k| {
                        corners[i][k] + (corners[j][k] - corners[i][k]) * 0.2
                    });
                    let points = vec![corners[i], tip];
                    // Over a dark stroke as the selection's are: bare, the
                    // white sank into pale grass.
                    lines.push(gpu::Line {
                        points: points.clone(),
                        width: 3.,
                        color: crate::colors::with_alpha(crate::colors::CASING, 0xcc),
                    });
                    lines.push(gpu::Line {
                        points,
                        width: 1.,
                        color: crate::colors::with_alpha(crate::colors::HANDLE, 0xd0),
                    });
                }
            }
        }
        lines
    }

    /// Hands the renderer the current guides. The selection's outline and
    /// markers follow the selection on their own (`sync_selection`).
    pub(super) fn push_overlay(&self) {
        self.renderer.set_overlay(self.overlay_lines());
    }

    /// For a frame without painting the panes: points view `view`'s
    /// canvas at its camera and refreshes the guides. False when the pane
    /// must paint after all (see `fast_frames`).
    pub(crate) fn sync_camera(&self, view: usize) -> bool {
        if !self.fast_frames(view) {
            return false;
        }
        let v = &self.views[view];
        v.viewport.set_camera(v.camera, self.extent());
        self.push_overlay();
        true
    }

    /// Whether view `view` can redraw a camera move or hover without
    /// painting its pane again: nothing the pane paints itself (gizmo,
    /// marquee, the harbour, river and slot tools' handles and labels,
    /// flow arrows) is showing, and no uploads need the pane's animation
    /// frames.
    pub(super) fn fast_frames(&self, view: usize) -> bool {
        self.gizmo_frame(view).is_none()
            && !matches!(self.views[view].drag, Some(Drag::Marquee(_)))
            && !matches!(self.tool, Tool::Harbour | Tool::River | Tool::Slot)
            && self.overlay() != Some(anno_formats::grids::Kind::WaterFlow)
            && self.lines_current()
            && !self.renderer.pending()
    }
}

impl Editor {
    /// The island viewports' cursor hint and frame readout, over the panes
    /// from the window root: they change every frame, and the panes stay
    /// cached while the camera moves.
    /// The cursor's ground point for the status bar, in a fixed slot so the
    /// counts beside it hold still; named so it is not taken for the
    /// selection's position in the Inspector.
    pub(crate) fn cursor_readout(&self) -> Option<Div> {
        let island = self
            .island
            .as_ref()
            .filter(|_| self.workspace == Workspace::Island)?;
        let [x, y, z] = island.cursor?;
        // As the Inspector writes positions: axis letters in their colours,
        // the unit once after the last value, each value in a fixed width so
        // the counts after it hold still; a rule, not a floating dot, after.
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                // Named, so it is not taken for the selection's position.
                .child("Cursor")
                .children(
                    // Each value with its unit, as the Inspector's fields.
                    [("X", x, " m"), ("Y", y, " m"), ("Z", z, " m")]
                        .into_iter()
                        .zip(theme::get().axes)
                        .map(|((axis, v, unit), color)| {
                            // Letter and value right-aligned together in one
                            // slot: aligned apart, the slack sat between a
                            // letter and its own value, pairing it with the
                            // one before.
                            div()
                                .w(px(58.))
                                .flex()
                                .justify_end()
                                .gap_1()
                                .child(div().text_color(color).child(axis))
                                .child(format!("{v:.1}{unit}"))
                        }),
                )
                .child(div().w(px(1.)).h(px(12.)).mx_1().bg(theme::get().border)),
        )
    }

    pub(crate) fn island_status(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(island) = &self.island else {
            return Vec::new();
        };
        let visible = self.dock_visible_panes();
        let mut out = Vec::new();
        for (view, pane) in [
            (0, crate::editor::Pane::IslandView),
            (1, crate::editor::Pane::IslandTop),
        ] {
            if !visible.contains(&pane) {
                continue;
            }
            let b = island.views[view].bounds;
            if b.size.width <= px(0.) {
                continue;
            }
            // The cursor's ground point is on the status bar: here it pushed
            // every key hint sideways as the mouse crossed the shore.
            let hint = String::new()
                + &island
                .cursor
                .and_then(|at| island.buildable_readout(at))
                .unwrap_or_default()
                + match island.tool {
                    // No W/E/R: the Move/Rotate/Scale chips above show them.
                    // F and Delete only with something to act on: offered
                    // beside a dimmed Frame they read as broken keys.
                    Tool::Select if island.selection.is_empty() => "Click or drag: select",
                    Tool::Select => "Click or drag: select · F: frame · Delete: remove",
                    // Nothing to place yet: the chip leads with the tool's own
                    // step, as every other tool's does, where it was bare
                    // camera controls.
                    Tool::Place if island.place.is_none() && island.place_object.is_none() => {
                        "Pick a model in the Library"
                    }
                    Tool::Place => "Click: place",
                    Tool::Paint => "Drag: paint · Alt-click or I: pipette · [ ]: radius",
                    Tool::Grid => {
                        "Drag: paint the grid · Shift: erase · Alt-click: pick value · [ ]: radius"
                    }
                    Tool::Scatter => "Drag: scatter the set · Shift: thin · [ ]: radius",
                    Tool::Tint => {
                        "Drag: tint · Shift: neutralize · Alt-click: pick tint · [ ]: radius"
                    }
                    Tool::Harbour => {
                        "Drag: trace the shore · Drag a point: move · Double-click: add · Shift-click: remove"
                    }
                    Tool::Slot => "Click: place a slot · Shift-click: remove",
                    Tool::River => {
                        "Click: add a point · Drag: move points and widths · Enter or double-click: carve · Delete: remove a point · Esc: cancel"
                    }
                    // Only the tools that use a target height sample one:
                    // advertised on Raise it promised what Raise ignores.
                    Tool::Flatten | Tool::Fill | Tool::Scrape => {
                        "Drag: sculpt · Shift: invert · Alt-click: target height · [ ]: radius"
                    }
                    Tool::Raise => "Drag: raise · Shift: lower · [ ]: radius",
                    Tool::Lower => "Drag: lower · Shift: raise · [ ]: radius",
                    _ => "Drag: sculpt · Shift: invert · [ ]: radius",
                }
                // The Top view does not tilt: there right-drag pans.
                // " | ": the camera keys are a quieter second group.
                + if island.views[view].camera.tilt > 0. {
                    " | Right-drag: orbit · Middle-drag: pan · Scroll: zoom"
                } else {
                    " | Right- or middle-drag: pan · Scroll: zoom"
                };
            // A tool with nothing to say leaves the camera part leading.
            let hint = hint
                .trim_start_matches(" · ")
                .trim_start_matches(" | ")
                .replace(" ·  · ", " · ");
            // Frame rate shown, then what the CPU and GPU frame times alone
            // would allow; the GPU's share is the viewport passes'.
            let stats = island.frame_stats();
            let fps = |ms: f32| if ms > 0. { 1000. / ms } else { 0. };
            let rate = format!(
                "{:.0} fps · CPU {:.1} ms ({:.0} fps: build {:.1}, layout {:.1}, encode {:.1}) · GPU {:.1} ms ({:.0} fps)",
                fps(stats.frame_ms),
                stats.cpu_ms,
                fps(stats.cpu_ms),
                stats.build_ms,
                stats.layout_ms,
                stats.encode_ms,
                stats.gpu_ms,
                fps(stats.gpu_ms),
            );
            out.push(
                div()
                    .absolute()
                    .left(b.origin.x)
                    .top(b.origin.y)
                    .w(b.size.width)
                    .h(b.size.height)
                    .child(
                        // On a dark pill: grey text on sea and grass is unreadable.
                        // Capped short of the file badge at the right, so a
                        // long tool hint (River's) wraps, between items only,
                        // instead of running under it.
                        widgets::hint_pill(&hint)
                            .absolute()
                            .bottom_2()
                            .left_3()
                            .max_w(b.size.width * 0.7),
                    )
                    // The Top view is for measuring: north and a metre scale.
                    .when(view == 1, |d| d.children(island.top_view_scale(b)))
                    // Perspective: the world's axes as the camera sees them,
                    // where Top keeps its scale; a click turns to face north.
                    .when(view == 0, |d| {
                        d.child(
                            div()
                                .id("view-orientation")
                                .absolute()
                                .bottom(px(40.))
                                .right_3()
                                .cursor_pointer()
                                .tooltip(Tooltip::text(
                                    "Orientation",
                                    "",
                                    "World axes as this view sees them; the notch on the rim is the map’s north. Click to face north.",
                                ))
                                .child(gpui_editor_kit::gizmo::orientation(island.views[0].camera, true))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(i) = &mut this.island {
                                        i.face_north(0);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .when(view == 0 && self.stats, |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom_2()
                                .right_3()
                                .px_1()
                                .rounded_sm()
                                .bg(theme::get().overlay)
                                .text_size(px(10.))
                                .text_color(theme::get().text)
                                .child(rate),
                        )
                    })
                    .into_any_element(),
            );
        }
        out
    }
}

/// A model name without its one- or two-letter variant token, at the end
/// or before a trailing word: `tree_maritime_pine_01_k` →
/// `tree_maritime_pine_01`, `tree_beech_02_a_soft` → `tree_beech_02_soft`.
/// The second form left "a soft", "b soft" and "d soft" as separate cards.
fn variant_base(name: &str) -> String {
    let token = |t: &str| (1..=2).contains(&t.len()) && t.chars().all(|c| c.is_ascii_lowercase());
    let parts: Vec<&str> = name.split('_').collect();
    match parts.as_slice() {
        [base @ .., last] if !base.is_empty() && token(last) => base.join("_"),
        [base @ .., v, word]
            if !base.is_empty()
                && token(v)
                && word.len() >= 3
                && word.chars().all(|c| c.is_ascii_lowercase()) =>
        {
            format!("{}_{word}", base.join("_"))
        }
        _ => name.to_owned(),
    }
}

/// An island file as the list names it. Shipped names repeat their
/// province (`celtic_island_medium_01` → `Medium 01`), which the list's
/// group header and the badge's tooltip already say.
/// The island list's kind filter, read from the shipped file names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IslandKind {
    All,
    /// The default: under All, decoration and campaign islands filled the
    /// first screen before any island one would start from.
    #[default]
    Playable,
    Decoration,
    Other,
}

impl IslandKind {
    const ALL: [IslandKind; 4] = [Self::All, Self::Playable, Self::Decoration, Self::Other];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Playable => "Playable",
            Self::Decoration => "Decoration",
            Self::Other => "Campaign & NPC",
        }
    }

    /// Names are the only signal the shipped files carry; "campaign" wins
    /// over "deco" so a campaign decoration counts as Other.
    fn of(path: &str) -> IslandKind {
        let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        if ["campaign", "pirate", "third_party", "thirdparty", "trader"]
            .iter()
            .any(|w| name.contains(w))
        {
            Self::Other
        } else if name.contains("deco") {
            Self::Decoration
        } else {
            Self::Playable
        }
    }

    pub(crate) fn admits(self, path: &str) -> bool {
        self == Self::All || Self::of(path) == self
    }
}

/// The content pack a shipped island comes from, `None` for the base game:
/// `data/dlc01/...` is "DLC 1". Tells "Medium 01" from DLC 1's Medium 01.
pub(crate) fn island_pack(path: &str) -> Option<String> {
    crate::browser::pack_name(path.split('/').nth(1)?)
}

pub(crate) fn island_title(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let name = file.trim_end_matches(".a7m");
    let short = name
        .find("_island_")
        .map_or(name, |i| &name[i + "_island_".len()..]);
    // The pack goes in the detail column (island_pack); in the name it
    // read as "Egypt dlc 03 valley" beside Assets' "DLC 3".
    let mut parts: Vec<&str> = short.split('_').collect();
    if let Some(i) = parts.iter().position(|p| p.starts_with("dlc")) {
        let tail_digits = parts[i].len() > 3;
        parts.remove(i);
        if !tail_digits
            && parts
                .get(i)
                .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
        {
            parts.remove(i);
        }
    }
    let mut out = words(&parts.join(" "));
    if let Some(first) = out.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// A game id as a modder reads it: `tree_beech_02_a` → `Tree beech 02 a`.
/// The raw id stays in tooltips and the inspector's path.
pub(crate) fn pretty(id: &str) -> String {
    let spaced = sentence_case(&words(&id.replace('_', " ")));
    // A variant letter joins the number before it ("Bush 06a marsh"): on
    // its own, "Bush 06 a marsh" read as English, a bush and "a marsh".
    // A word run into its number splits ("detail01" → "detail 01",
    // "var0" → "var 0"); a short code such as "06a" or "a7" stays whole.
    let split: Vec<String> = spaced
        .split(' ')
        .flat_map(|w| {
            let letters = w.chars().take_while(char::is_ascii_alphabetic).count();
            let rest = &w[letters..];
            if letters >= 3 && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                vec![w[..letters].to_owned(), rest.to_owned()]
            } else {
                vec![w.to_owned()]
            }
        })
        .collect();
    let mut out = String::new();
    let mut last_numeric = false;
    for w in split.iter().map(String::as_str) {
        let letter = w.len() == 1 && w.chars().all(|c| c.is_ascii_lowercase());
        if !(letter && last_numeric) && !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
        last_numeric = w.chars().last().is_some_and(|c| c.is_ascii_digit());
    }
    if let Some(first) = out.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// Title Case asset texts (`Mountain Slot`) as the rest of the editor
/// writes: later words lower case unless they are place names.
fn sentence_case(text: &str) -> String {
    const PROPER: &[&str] = &["Albion", "Latium", "Celtic", "Roman", "Prydein"];
    text.split(' ')
        .enumerate()
        .map(|(i, w)| {
            let title = w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && w.chars().skip(1).all(|c| c.is_ascii_lowercase());
            if i > 0 && title && !PROPER.contains(&w) {
                w.to_ascii_lowercase()
            } else {
                w.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A ground layer's name as words: `GrassDry` → `dry grass`.
fn ground_name(layer: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for c in layer.chars() {
        if c.is_ascii_uppercase() || words.is_empty() {
            words.push(String::new());
        }
        if let Some(w) = words.last_mut() {
            w.push(c.to_ascii_lowercase());
        }
    }
    // The game names the kind first (`GrassDry`); English puts it last.
    if words.len() == 2 && matches!(words[1].as_str(), "dry" | "green" | "wet" | "high") {
        words.swap(0, 1);
    }
    words.join(" ")
}

/// Words the game's ids run together, spelled out. Matched per word
/// without case; anything else passes through.
const JOINED: &[(&str, &str)] = &[
    ("pirateoutpost", "pirate outpost"),
    ("legioneagle", "legion eagle"),
    ("workareaslot", "work area slot"),
    ("3rdparty", "third-party"),
    ("marshveg", "marsh"),
    ("noskirt", "no skirt"),
    ("simplecfg", "model object"),
    // Place names keep their capital wherever they fall in an id.
    ("prydein", "Prydein"),
    ("albion", "Albion"),
    ("latium", "Latium"),
    ("celtic", "Celtic"),
    ("roman", "Roman"),
];

fn words(text: &str) -> String {
    text.split(' ')
        .map(|w| {
            JOINED
                .iter()
                .find(|(joined, _)| w.eq_ignore_ascii_case(joined))
                .map_or(w, |(_, spelled)| spelled)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A section heading that says its value while folded ("Wetlands 01 ·
/// Celtic 01" under "Ground cover & lighting"). Open, the rows say it.
fn fold_summary(heading: Stateful<Div>, open: bool, summary: String) -> Div {
    let out = div().flex().flex_col().child(heading);
    if open || summary.is_empty() {
        return out;
    }
    // Its own muted line under the heading text: beside the heading, the
    // longest value ("Wetlands 01 · Celtic 01") cut to "Wetlan…" next to
    // the eye. Pulled up into the heading's padding so the pair reads as
    // one row.
    out.child(
        div()
            .pl(px(14.))
            .mt(px(-5.))
            .pb_1()
            .min_w_0()
            .truncate()
            .text_size(px(11.))
            .text_color(theme::get().muted)
            .child(summary),
    )
}

/// A preset id without its kind prefix, capitalised: `ls_wetlands_01` →
/// `Wetlands 01`, `amb_celtic_01` → `Celtic 01`.
fn preset_name(id: &str) -> String {
    let bare = ["ls_", "amb_"]
        .iter()
        .find_map(|p| id.strip_prefix(p))
        .unwrap_or(id);
    let mut out = pretty(bare);
    if let Some(first) = out.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    out
}

fn mesh_name(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

impl Editor {
    /// Loads the shipped records the harbour and slot tools copy, for the
    /// open island's province. About 0.1–0.5 s; done once per island.
    /// Reads the shipped harbour and slot records off the UI thread once the
    /// harbour, resource or river tool is out. Read on a click it froze the
    /// window for seconds, on any button, so an orbit's right-press stalled;
    /// read after a failure, it froze again on every click.
    fn start_harbour_library(&mut self, cx: &mut Context<Self>) {
        let (Some(install), Some(assets), Some(island)) =
            (self.install.clone(), self.assets.clone(), &mut self.island)
        else {
            return;
        };
        if !island.needs_harbour_library() || island.harbour_loading || island.harbour_failed {
            return;
        }
        island.harbour_loading = true;
        let (region, path) = (island.region(), island.path.clone());
        cx.spawn(async move |this, cx| {
            let lib = cx
                .background_spawn(async move {
                    let _span = gpui_editor_kit::profile::span("harbour records (background)");
                    harbour::Library::load(&install, &assets, &region)
                })
                .await;
            this.update(cx, |this, cx| {
                // Another island may have opened meanwhile.
                if let Some(i) = &mut this.island
                    && i.path == path
                {
                    i.harbour_loading = false;
                    match lib {
                        Ok(lib) if i.harbour.is_none() => i.harbour = Some(Arc::new(lib)),
                        Ok(_) => {}
                        Err(e) => {
                            i.harbour_failed = true;
                            i.say(format!("Harbour records unavailable: {e:#}"), true);
                        }
                    }
                }
                this.drain_messages();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The same records read at once, for a save that needs them now.
    pub fn load_harbour_library(&mut self) {
        let (Some(install), Some(assets), Some(island)) =
            (&self.install, &self.assets, &mut self.island)
        else {
            if let Some(i) = &mut self.island {
                i.say("Assets are still loading; try again in a moment", true);
            }
            return;
        };
        if island.harbour.is_some() {
            return;
        }
        match harbour::Library::load(install, assets, &island.region()) {
            Ok(lib) => island.harbour = Some(Arc::new(lib)),
            Err(e) => island.say(format!("Harbour records unavailable: {e:#}"), true),
        }
    }

    /// Loads the shipped sound spot and blocker records once, then runs the
    /// auto placement: sound spots, or build blockers when `blockers`.
    pub fn auto_spots(&mut self, blockers: bool) {
        let (Some(install), Some(island)) = (&self.install, &mut self.island) else {
            return;
        };
        if island.assets.is_none() {
            return island.say("Assets are still loading; try again in a moment", true);
        }
        if island.spot_records.is_none() {
            let mut guids = sound::guids(self.game).to_vec();
            guids.push(super::blockers::BLOCKER);
            match harbour::find_records(install, &guids) {
                Ok(r) => island.spot_records = Some(Arc::new(r)),
                Err(e) => return island.say(format!("Spot records unavailable: {e:#}"), true),
            }
        }
        if blockers {
            island.auto_blockers(install);
        } else {
            island.auto_sound_spots();
        }
    }

    /// Harbour tool: a regular spot or the island's start coast.
    fn island_harbour_options(
        &self,
        mut options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        for (start, label, detail) in [
            (
                false,
                "Harbour spot",
                "Where players can found a settlement",
            ),
            (
                true,
                "Start coast",
                "Where the player’s first trading post goes; one per island, Large or XL islands",
            ),
        ] {
            options = options.child(
                button(label, island.start_coast == start)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text(label, "", detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.start_coast = start;
                        }
                        cx.notify();
                    })),
            );
        }
        options = self.island_auto_options(options.child(widgets::toolbar_separator()), cx);
        if island.harbour_selected() {
            options = options.child(widgets::toolbar_separator()).child(
                button("Flip side", false)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text(
                        "Flip side",
                        "",
                        "Turn the selected harbour’s sea side around",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.flip_selected_coasts();
                        }
                        cx.notify();
                    })),
            );
        }
        options
    }

    /// Draw by hand, or drag a region and let the tool place `auto_count`.
    fn island_auto_options(
        &self,
        mut options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        let draw = if island.tool == Tool::Harbour {
            (
                "Draw",
                "Drag along the shore; a click places 48 m. Drag points to reshape, double-click adds one",
            )
        } else {
            ("Place", "Click to place one")
        };
        for (auto, (label, detail)) in [
            (false, draw),
            (
                true,
                (
                    "Auto in region",
                    "Drag a rectangle; the tool places the best spots inside it",
                ),
            ),
        ] {
            options = options.child(
                button(label, island.auto_region == auto)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text(label, "", detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.auto_region = auto;
                        }
                        cx.notify();
                    })),
            );
        }
        if island.auto_region {
            let step = |delta: i32| {
                cx.listener(
                    move |this: &mut Self,
                          _: &ClickEvent,
                          _: &mut Window,
                          cx: &mut Context<Self>| {
                        if let Some(i) = &mut this.island {
                            i.auto_count = (i.auto_count as i32 + delta).clamp(1, 12) as u32;
                        }
                        cx.notify();
                    },
                )
            };
            options = options
                .child(button("−", false).text_size(px(11.)).on_click(step(-1)))
                .child(
                    div()
                        .px_1()
                        .text_size(px(11.))
                        .child(format!("up to {}", island.auto_count)),
                )
                .child(button("+", false).text_size(px(11.)).on_click(step(1)));
        }
        options
    }

    /// River tool: what new drafts are, and finishing or dropping the
    /// current one.
    fn island_river_options(
        &self,
        mut options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        for (lake, label, detail) in [
            (
                false,
                "River",
                "An open curve that runs downhill, with a width per point",
            ),
            (true, "Lake", "A closed curve, flat at its lowest point"),
        ] {
            options = options.child(
                button(label, island.lake_mode == lake)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text(label, "", detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.lake_mode = lake;
                            // A new draft follows the switch; a re-edited
                            // object keeps what it is.
                            if let Some(d) = i.river.as_mut().filter(|d| d.id.is_none()) {
                                d.lake = lake;
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        let Some(draft) = &island.river else {
            return options;
        };
        options
            .child(widgets::toolbar_separator())
            .child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .child(format!("{} points", draft.knots.len())),
            )
            .child(
                button("Carve", false)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text(
                        "Carve",
                        "Enter",
                        "Write the curve, carve its bed and paint River and Water cells, as one undo step",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.finish_river();
                        }
                        this.drain_messages();
                        cx.notify();
                    })),
            )
            .child(
                button("Cancel", false)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text("Cancel", "Esc", "Drop the draft"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.river = None;
                        }
                        cx.notify();
                    })),
            )
    }

    /// Slot tool: which resource.
    fn island_slot_options(
        &self,
        mut options: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let island = self.island.as_ref().expect("options render with an island");
        for (resource, label) in harbour::Resource::ALL {
            let shipped = island
                .harbour
                .as_ref()
                .is_none_or(|lib| lib.guid(resource.pieces(false).0).is_some());
            if !shipped {
                continue;
            }
            options = options.child(
                button(label, island.resource == resource)
                    .text_size(px(11.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.resource = resource;
                        }
                        cx.notify();
                    })),
            );
        }
        options = options.child(widgets::toolbar_separator());
        // Marsh slots have no fixed variant; shipped ones are all listed and
        // carry no generator slot count.
        let kinds: &[(bool, &str, &str)] = if island.resource.has_fixed()
            && island
                .harbour
                .as_ref()
                .is_none_or(|lib| lib.guid(island.resource.pieces(true).0).is_some())
        {
            &[
                (
                    false,
                    "Random",
                    "The map generator turns some of the island’s random slots into mines, more on larger islands",
                ),
                (
                    true,
                    "Always",
                    "A fixed slot: always a mine (or river spot)",
                ),
            ]
        } else {
            &[]
        };
        for &(fixed, label, detail) in kinds {
            options = options.child(
                button(label, island.fixed_slots == fixed)
                    .text_size(px(11.))
                    .tooltip(Tooltip::text(label, "", detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.fixed_slots = fixed;
                        }
                        cx.notify();
                    })),
            );
        }
        options = self.island_auto_options(options.child(widgets::toolbar_separator()), cx);
        // Only project islands reach the test map.
        if let (Some((id, kind)), Some(assets), true) =
            (island.selected_slot(), &island.assets, island.is_new())
        {
            let current = island.listing.slot_resources.get(&id).copied();
            options = options.child(widgets::toolbar_separator()).child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("Test map:"),
            );
            let slots = island.game.profile().slots;
            let deposits = match slots {
                ResourceSlots::Random => island
                    .doc
                    .objects
                    .iter()
                    .find(|o| o.id == id)
                    .and_then(|o| assets.get(o.guid))
                    .into_iter()
                    .flat_map(|a| assets.slot_replacements(a.guid, &island.region()))
                    .map(|a| fertility::Deposit {
                        guid: a.guid,
                        label: fertility::label(assets, a.guid),
                    })
                    .collect(),
                ResourceSlots::Typed => {
                    fertility::deposits(assets, &island.region(), kind.slot_type())
                }
            };
            // A random slot can also be replaced by nothing.
            let empty = (slots == ResourceSlots::Random).then_some((Some(0), "Empty".to_owned()));
            let picks = std::iter::once((None, "Random".to_owned()))
                .chain(empty)
                .chain(deposits.into_iter().map(|d| (Some(d.guid), d.label)));
            for (deposit, label) in picks {
                options = options.child(
                    button(label.clone(), current == deposit)
                        .text_size(px(11.))
                        .tooltip(Tooltip::text(
                            label,
                            "",
                            crate::game_text::of(island.game).slot_resource,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(i) = &mut this.island {
                                i.set_slot_resource(deposit);
                            }
                            cx.notify();
                        })),
                );
            }
        }
        options
    }
}

/// Whose settings [`Editor::listing_controls`] edits.
#[derive(Clone, Copy)]
pub enum ListingOf {
    Wizard,
    Island,
}

impl Editor {
    fn edit_listing(&mut self, of: ListingOf, f: impl FnOnce(&mut Listing)) {
        match of {
            ListingOf::Wizard => {
                if let Some(w) = &mut self.wizard {
                    f(w.listing_mut());
                }
            }
            ListingOf::Island => {
                if let Some(i) = &mut self.island {
                    i.set_listing(f);
                }
            }
        }
    }

    /// Role, size class and where the game offers a new island. The wizard
    /// has no land yet, so it warns from the map size; the inspector shows
    /// the class the land gives and allows an override.
    pub fn listing_controls(&self, of: ListingOf, cx: &mut Context<Self>) -> Div {
        let profile = self.game.profile();
        let (min_start, max_slot, start_classes) = (
            profile.min_start_extent(),
            profile.max_slot(),
            profile.start_classes(),
        );
        let (listing, land) = match of {
            ListingOf::Wizard => {
                let Some(w) = &self.wizard else { return div() };
                (w.listing().clone(), None)
            }
            ListingOf::Island => {
                let Some(i) = &self.island else { return div() };
                (i.listing.clone(), Some((i.land_extent(), i.land_class())))
            }
        };
        let note = |text: String, warn: bool| {
            div()
                .text_size(px(11.))
                .text_color(if warn {
                    theme::get().error
                } else {
                    theme::get().muted
                })
                .child(text)
        };
        // In the wizard the map's width is known before the land is: a map
        // too narrow for a start island offers the start roles dimmed with
        // why, rather than warning only once one was picked. A role already
        // chosen stays live, so it can be seen and changed.
        let narrow = matches!(of, ListingOf::Wizard)
            && self.wizard.as_ref().is_some_and(|w| w.world() < min_start);
        let current = listing.kind.clone();
        // A width the row offers, not the threshold: "321 m" read as an
        // off-by-one beside the 320 and 384 buttons.
        let start_width = self
            .wizard
            .as_ref()
            .and_then(|w| w.first_width_from(min_start))
            .map(|(_, w)| w);
        // A start role on a map too narrow for it widens the map to the
        // first width that takes one, in the same click: dimmed, the roles
        // were a dead end with the way out in a link two lines down. Only
        // where no width of the region is wide enough is it off.
        let widen = self
            .wizard
            .as_ref()
            .and_then(|w| w.first_width_from(min_start))
            .map(|(index, _)| index);
        let roles = widgets::segmented_where(
            "listing-role",
            // Bare names: "Start island → 384 m map" on two of three
            // segments said the width three times with the line below; the
            // line alone announces the widening.
            crate::project::ROLES.map(|(label, kind)| (kind.to_owned(), label.into())),
            &listing.kind,
            move |kind| {
                (narrow && kind != "Normal" && *kind != current && start_width.is_none())
                    .then(|| "No map of this region is wide enough for a start island".into())
            },
            cx,
            move |this, kind, _, _| {
                if narrow
                    && kind != "Normal"
                    && let (Some(index), Some(w)) = (widen, &mut this.wizard)
                {
                    w.choose_size(index);
                }
                this.edit_listing(of, |l| l.kind = kind)
            },
        );
        // The explanation sits under its checkbox, not across the dialog.
        let check = |label: &'static str, on: bool, detail: &'static str, f: fn(&mut Listing)| {
            div()
                .flex()
                .flex_col()
                .child(
                    checkbox(label, on, "").on_click(cx.listener(move |this, _, _, cx| {
                        this.edit_listing(of, f);
                        cx.notify();
                    })),
                )
                .child(div().pl(px(22.)).child(hint(detail)))
        };
        let mut out = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(note("Role".into(), false))
            .child(roles)
            .child(note(
                match listing.kind.as_str() {
                    // The widening said in words: the arrow on the role
                    // alone was easy to miss.
                    "Normal" => match start_width {
                        // The width it goes to; the current one is selected
                        // just above.
                        Some(w) if narrow => format!(
                            "Fills non-start slots. Choosing a start role widens this map to {w} m."
                        ),
                        _ => "Non-start island slots only.".to_owned(),
                    },
                    "Starter" => "Only where a player starts.".to_owned(),
                    _ => "Either, like every shipped start island.".to_owned(),
                },
                false,
            ));
        let class = match land {
            None => {
                let world = self.wizard.as_ref().map_or(0, |w| w.world());
                if listing.is_start() && world < min_start {
                    out = out.child(note(
                        format!("A {world} m map is too small for a start island: starts need {min_start}–{max_slot} m of land ({start_classes})"),
                        true,
                    ));
                }
                // The wizard says where the size class comes from under
                // Map width, where it is asked about.
                None
            }
            Some((extent, class)) => {
                let mut sizes = div().flex().flex_wrap().gap_1().child(
                    widgets::button(format!("Auto: {class}"), listing.size_override.is_none())
                        .text_size(px(12.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit_listing(of, |l| l.size_override = None);
                            cx.notify();
                        })),
                );
                let classes = if profile.placeholder_size {
                    profile.size_classes
                } else {
                    &[]
                };
                for name in classes.iter().map(|c| c.name) {
                    sizes = sizes.child(
                        widgets::button(name, listing.size_override.as_deref() == Some(name))
                            .text_size(px(12.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_listing(of, |l| l.size_override = Some(name.to_owned()));
                                cx.notify();
                            })),
                    );
                }
                out = out
                    .child(note(format!("Size class · land {extent} m"), false))
                    .child(sizes);
                if extent > max_slot {
                    out = out.child(note(
                        format!("{extent} m of land is wider than any slot ({max_slot} m); it may overlap neighbours"),
                        true,
                    ));
                }
                Some(listing.size_override.clone().unwrap_or(class.to_owned()))
            }
        };
        if let Some(class) = class
            && listing.is_start()
            && !matches!(class.as_str(), "Large" | "XL")
        {
            out = out.child(note(
                format!("{class} islands never fill start slots; starts are {start_classes} ({min_start}–{max_slot} m of land)"),
                true,
            ));
        }
        out = out.child(check(
            "In the random pool",
            listing.in_pool,
            "Normal games may pick it.",
            |l| l.in_pool = !l.in_pool,
        ));
        if profile.fertility_choice
            && let ListingOf::Island = of
            && let (Some(island), Some(assets)) = (&self.island, &self.assets)
        {
            let fixed = listing.fertilities.is_some();
            let modes = div()
                .flex()
                .gap_1()
                .child(
                    button("Random", !fixed)
                        .text_size(px(11.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit_listing(of, |l| l.fertilities = None);
                            cx.notify();
                        })),
                )
                .child(
                    button("Choose", fixed)
                        .text_size(px(11.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit_listing(of, |l| {
                                l.fertilities.get_or_insert_with(Vec::new);
                            });
                            cx.notify();
                        })),
                );
            out = out
                .child(note("Test-map fertilities".into(), false))
                .child(modes);
            if let Some(selected) = &listing.fertilities {
                let mut choices = div().flex().flex_wrap().gap_1();
                for f in fertility::choices(assets, &island.region()) {
                    let guid = f.guid;
                    choices = choices.child(
                        button(f.label, selected.contains(&guid))
                            .text_size(px(11.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_listing(of, |l| {
                                    if let Some(list) = &mut l.fertilities {
                                        if list.contains(&guid) {
                                            list.retain(|g| *g != guid);
                                        } else {
                                            list.push(guid);
                                        }
                                    }
                                });
                                cx.notify();
                            })),
                    );
                }
                out = out.child(choices);
                if selected.is_empty() {
                    out = out.child(note("No fertilities selected".into(), false));
                }
            }
        }
        out.child(check(
            "On the test map",
            listing.on_test_map,
            // The command by its key: the menu item's name changes while the
            // game copy is out of date.
            "Used by Test in game (F5)",
            |l| l.on_test_map = !l.on_test_map,
        ))
    }
}

/// Library rows drawn at most; the search narrows the rest.
const LIBRARY_ROWS: usize = 1000;

/// A Library card dragged toward a viewport: what it places, and the
/// picture and name its ghost shows under the cursor.
#[derive(Clone)]
pub(crate) struct LibraryDrag {
    pick: Pick,
    label: SharedString,
    image: Option<Arc<RenderImage>>,
}

impl Render for LibraryDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .p_1()
            .pr_2()
            .rounded(px(4.))
            .bg(theme::get().popup)
            .border_1()
            .border_color(theme::get().floating_border)
            .shadow_lg()
            .opacity(0.92)
            .text_size(px(11.))
            .text_color(theme::get().text)
            .child(
                div()
                    .size(px(32.))
                    .rounded(px(3.))
                    .bg(theme::get().picture_bg)
                    .overflow_hidden()
                    .children(self.image.clone().map(|i| img(i).size_full())),
            )
            .child(self.label.clone())
    }
}

/// What a Library card places.
#[derive(Clone)]
enum Pick {
    Prop(String),
    /// Opens a variant group.
    Group(String),
    /// The object's island, GUID and model.
    Object(String, u32, Option<String>),
}

impl Editor {
    /// Builds or loads the placeable catalog off the UI thread, once.
    fn ensure_catalog(&mut self, cx: &mut Context<Self>) {
        let (None, false, Some(install)) =
            (&self.catalog, self.catalog_loading, self.install.clone())
        else {
            return;
        };
        self.catalog_loading = true;
        let cache = self.config.join("catalog.json");
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move { super::catalog::Catalog::load_or_build(&install, &cache) })
                .await;
            this.update(cx, |this, cx| {
                this.catalog_loading = false;
                match built {
                    Ok(c) => this.catalog = Some(Arc::new(c)),
                    Err(e) => this.message(format!("Library unavailable: {e:#}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn thumbs_pending(&self) -> bool {
        let source = self.import.as_ref().and_then(|w| w.source_thumbs.as_ref());
        [self.thumbs.as_ref(), source]
            .into_iter()
            .flatten()
            .any(|t| t.pending())
    }

    /// The Library's Props tab, unfiltered by group.
    pub(crate) fn browse_game_props(&mut self) {
        if let Some(i) = &mut self.island {
            i.library_tab = super::catalog::Kind::Props;
            i.library_group = None;
        }
    }

    /// Picks up finished thumbnails every 100 ms while any are pending.
    pub(crate) fn poll_thumbs(&mut self, cx: &mut Context<Self>) {
        if self.thumbs_polling || !self.thumbs_pending() {
            return;
        }
        self.thumbs_polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let more = this.update(cx, |this, cx| {
                    // The Library's thumbnails and, while importing, the
                    // other game's.
                    let source = this.import.as_mut().and_then(|w| w.source_thumbs.as_mut());
                    let got = [this.thumbs.as_mut(), source]
                        .into_iter()
                        .flatten()
                        .fold(false, |got, t| t.receive() | got);
                    // Only the panes that show them, unless an import is
                    // open: notifying the editor redrew every pane ten times
                    // a second while a large island's thumbnails came in.
                    if got && this.import.is_some() {
                        cx.notify();
                    } else if got {
                        // The Models page and the Inspector's preset pictures
                        // too: left out, the Models page kept its "…" tiles
                        // with every thumbnail ready.
                        for pane in [
                            crate::editor::Pane::IslandLibrary,
                            crate::editor::Pane::IslandHierarchy,
                            crate::editor::Pane::IslandList,
                            crate::editor::Pane::IslandInspector,
                            crate::editor::Pane::ProjectModels,
                            crate::editor::Pane::ProjectOverview,
                        ] {
                            this.notify_pane(pane, cx);
                        }
                    }
                    let more = this.thumbs_pending();
                    // Every look is in: match the import's props by look.
                    if !more && this.import.as_ref().is_some_and(|w| w.needs_match()) {
                        this.import_remap();
                        cx.notify();
                    }
                    this.thumbs_polling = more;
                    more
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Replaces the scatter set with this region's six most used shipped
    /// props of a kind (Trees, Bushes, Rocks).
    fn scatter_preset(&mut self, kind: &str) {
        let Some(catalog) = self.catalog.clone() else {
            self.message("The Library is still collecting props", true);
            return;
        };
        let Some(island) = &mut self.island else {
            return;
        };
        let region = format!("/{}/", island.region().to_lowercase());
        let wanted = |path: &str| -> bool {
            let p = path.to_lowercase().replace('\\', "/");
            let stem = p.rsplit('/').next().unwrap_or(&p);
            match kind {
                // Harvestable kinds only: no dead trees, stumps or palms.
                "Trees" if island.game.profile().harvestable_tree_preset => {
                    anno_formats::landscape::is_harvestable_tree(island.game, &p)
                }
                "Trees" => {
                    p.contains("/vegetation/trees/")
                        && !["dead", "stump", "palm", "log"]
                            .iter()
                            .any(|w| stem.contains(w))
                }
                "Bushes" => p.contains("/vegetation/") && stem.starts_with("bush"),
                _ => p.contains("/rocks/") && !stem.contains("cliff"),
            }
        };
        // Catalog props are most used first.
        let pick = |in_region: bool| -> Vec<String> {
            catalog
                .props
                .iter()
                .filter(|p| {
                    wanted(&p.path) && (!in_region || p.path.to_lowercase().contains(&region))
                })
                .take(6)
                .map(|p| p.path.replace('\\', "/"))
                .collect()
        };
        let mut paths = pick(true);
        if paths.is_empty() {
            paths = pick(false);
        }
        if paths.is_empty() {
            island.say(format!("No shipped {kind} props found"), true);
            return;
        }
        island.set_scatter(&paths);
        island.scatter.mark_trees = kind == "Trees";
        island.say(format!("Scatter set: {} {kind} props", paths.len()), false);
    }

    /// Loads the record of the first `guid` object (with `config`, for
    /// decorations) on shipped `island` and places copies of it.
    fn choose_catalog_object(&mut self, island: &str, guid: u32, config: Option<String>) {
        let Some(install) = self.install.clone() else {
            return;
        };
        let found = (|| -> anyhow::Result<Option<anno_formats::objects::Template>> {
            let archive = rda::Archive::from_bytes(install.read(island)?, island)?;
            let gamedata = anno_formats::adf::Adf::decode(&archive.read_path("gamedata.data")?)?;
            let store = objects::Objects::read(&gamedata)?;
            Ok(store
                .objects
                .iter()
                .position(|o| {
                    o.guid == guid && o.config.as_deref().map(|c| c.replace('\\', "/")) == config
                })
                .map(|i| store.template(i)))
        })();
        match (found, &mut self.island) {
            (Ok(Some(t)), Some(i)) => {
                // Stored the shipped way, with backslashes.
                i.choose_object(t, config.map(|c| c.replace('/', "\\")));
            }
            (Ok(None), _) => self.message("That object is no longer on its island", true),
            (Err(e), _) => self.message(format!("Could not read {island}: {e:#}"), true),
            _ => {}
        }
    }

    /// A Library card click: the Scatter tool collects props into its set;
    /// every other tool places one.
    fn pick_library(&mut self, pick: &Pick) {
        match pick {
            Pick::Group(base) => {
                if let Some(i) = &mut self.island {
                    i.library_group = Some(base.clone());
                }
            }
            Pick::Prop(path) => {
                let Some(i) = &mut self.island else { return };
                if i.tool == Tool::Scatter {
                    let added = i.toggle_scatter(path);
                    i.say(
                        format!(
                            "{} {} the scatter set ({} props)",
                            mesh_name(path),
                            if added { "added to" } else { "removed from" },
                            i.scatter.files.len()
                        ),
                        false,
                    );
                } else {
                    i.choose_prop(path);
                }
            }
            Pick::Object(island, guid, config) => {
                self.choose_catalog_object(island, *guid, config.clone())
            }
        }
    }

    /// Every shipped prop model, decoration and object, searchable; a click
    /// arms the Place tool with it.
    pub fn render_island_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_catalog(cx);
        let Some(island) = &self.island else {
            return crate::editor::pane_note("Open an island to place models from its region.");
        };
        let Some(catalog) = self.catalog.clone() else {
            return empty_pane("Collecting every shipped prop and object…");
        };
        // While the Inspector offers to add this island to the mod, that is
        // the screen's filled button; the Library's own steps back.
        let island_cta = !self.project.islands.iter().any(|i| i.path == island.path);
        let assets = self.assets.clone();
        let kind = island.library_tab;
        let query = island.library_filter.to_lowercase();
        let stem = |path: &str| mesh_name(path).to_owned();
        let folder = |path: &str| {
            let p = path.replace('\\', "/");
            let mut parts = p.rsplit('/').skip(1);
            let last = parts.next().unwrap_or("");
            // Model files sit in a folder of their own name; show its parent.
            if p.contains(&format!("/{last}/{last}")) {
                parts.next().unwrap_or(last).to_owned()
            } else {
                last.to_owned()
            }
        };
        let placing_prop = island
            .place
            .and_then(|f| island.doc.meshes.get(f as usize).cloned());
        let placing_object = island
            .place_object
            .as_ref()
            .map(|(t, c)| (t.object.guid, c.clone().map(|c| c.replace('\\', "/"))));
        // (label, detail, uses, action, active, model cfg)
        let mut rows: Vec<(String, String, u32, Pick, bool, Option<String>)> = Vec::new();
        let mod_models = self.model_entries();
        match kind {
            super::catalog::Kind::Mod => {
                for m in &mod_models {
                    rows.push((
                        stem(&m.cfg),
                        m.source.label().to_owned(),
                        0,
                        Pick::Prop(m.cfg.clone()),
                        placing_prop
                            .as_deref()
                            .is_some_and(|p| p.eq_ignore_ascii_case(&m.cfg)),
                        Some(m.cfg.clone()),
                    ));
                }
                // By the name on the card, as the Models page lists them.
                rows.sort_by_cached_key(|r| pretty(&r.0).to_lowercase());
            }
            super::catalog::Kind::Props => {
                for p in &catalog.props {
                    rows.push((
                        stem(&p.path),
                        folder(&p.path),
                        p.uses,
                        Pick::Prop(p.path.clone()),
                        placing_prop
                            .as_deref()
                            .is_some_and(|m| m.eq_ignore_ascii_case(&p.path)),
                        Some(p.path.replace('\\', "/")),
                    ));
                }
            }
            super::catalog::Kind::Decorations => {
                for d in &catalog.decorations {
                    rows.push((
                        stem(&d.config),
                        folder(&d.config),
                        d.uses,
                        Pick::Object(d.island.clone(), d.guid, Some(d.config.clone())),
                        placing_object == Some((d.guid, Some(d.config.clone()))),
                        Some(d.config.clone()),
                    ));
                }
            }
            super::catalog::Kind::Objects => {
                for o in &catalog.objects {
                    let asset = assets.as_deref().and_then(|a| a.get(o.guid));
                    rows.push((
                        object_name(assets.as_deref(), o.guid),
                        asset.map_or(String::new(), |a| a.template.clone()),
                        o.uses,
                        Pick::Object(o.island.clone(), o.guid, None),
                        placing_object == Some((o.guid, None)),
                        asset
                            .and_then(|a| a.cfgs.first())
                            .map(|c| c.replace('\\', "/")),
                    ));
                }
            }
        }
        // The tab's size before the search, for "16 of 25 models".
        let unfiltered = rows.len();
        rows.retain(|(label, detail, ..)| {
            query.is_empty()
                || label.to_lowercase().contains(&query)
                || detail.to_lowercase().contains(&query)
        });
        // The open island's own props first, in their usual order after:
        // a Celtic island opened on Egyptian palms. A stable sort, so each
        // half keeps the catalog's order.
        let mut on_island = 0;
        let here: std::collections::HashSet<String> = island
            .doc
            .meshes
            .iter()
            .map(|m| m.replace('\\', "/").to_ascii_lowercase())
            .collect();
        if kind == super::catalog::Kind::Props {
            let here_too = |r: &(String, String, u32, Pick, bool, Option<String>)| {
                r.5.as_deref()
                    .is_some_and(|c| here.contains(&c.to_ascii_lowercase()))
            };
            on_island = rows.iter().filter(|r| here_too(r)).count();
            if island.library_here {
                rows.retain(|r| here_too(r));
            } else {
                rows.sort_by_key(|r| !here_too(r));
            }
        }
        // Variants (`…_01_a`, `…_01_b`) share one card until opened; a
        // search shows them all, since it names what it wants.
        let group = island.library_group.clone();
        let grouping = query.is_empty() && kind != super::catalog::Kind::Objects;
        let models = rows.len();
        if grouping {
            match &group {
                Some(g) => rows.retain(|r| variant_base(&r.0) == *g),
                None => {
                    // One pass for each group's first row and size, one to
                    // emit them in catalog order.
                    let mut groups: std::collections::HashMap<String, (usize, u32)> =
                        std::collections::HashMap::with_capacity(rows.len());
                    for (n, r) in rows.iter().enumerate() {
                        groups.entry(variant_base(&r.0)).or_insert((n, 0)).1 += 1;
                    }
                    let mut grouped = Vec::with_capacity(groups.len());
                    for (n, r) in rows.iter().enumerate() {
                        let base = variant_base(&r.0);
                        let (first, count) = groups[&base];
                        if first != n {
                            continue;
                        }
                        let mut row = r.clone();
                        if count > 1 {
                            row.1 = format!("{count} variants · click to see them");
                            row.3 = Pick::Group(base.clone());
                            row.4 = false;
                            row.0 = base;
                        }
                        grouped.push(row);
                    }
                    rows = grouped;
                }
            }
        }
        let total = rows.len();
        rows.truncate(LIBRARY_ROWS);
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            let dir = self.config.join("thumbnails");
            self.thumbs = Some(super::catalog::Thumbs::start(install, dir));
        }
        if let Some(thumbs) = &mut self.thumbs {
            thumbs.request(rows.iter().filter_map(|r| r.5.as_deref()));
        }
        self.poll_thumbs(cx);
        let Some(island) = &self.island else {
            return crate::editor::pane_note("Open an island to place models from its region.");
        };

        // The game's own split, explained on hover: the names alone needed
        // knowing how islands store what is on them.
        let tabs = widgets::segmented_tips(
            "library-tabs",
            [
                (
                    super::catalog::Kind::Props,
                    "Props",
                    "Bushes, rocks and trees in the island’s prop grid: many light copies, no gameplay.",
                ),
                (
                    super::catalog::Kind::Decorations,
                    "Decorations",
                    "Models placed as a generic game object, as most shipped scenery is.",
                ),
                (
                    super::catalog::Kind::Objects,
                    "Objects",
                    "Game objects by asset: slots, markers, harbours and effects, copied from a shipped island.",
                ),
                (
                    super::catalog::Kind::Mod,
                    "This mod",
                    "Models this mod and its dependencies carry.",
                ),
            ]
            // No counts: the catalog counts files while the grid counts
            // models, and an empty This mod says so in its own empty state.
            .map(|(k, label, tip)| (k, label.into(), tip)),
            &kind,
            cx,
            |this, k, _, _| {
                if let Some(i) = &mut this.island {
                    i.library_tab = k;
                    i.library_group = None;
                }
            },
        );
        let empty_mod_tab =
            total == 0 && kind == super::catalog::Kind::Mod && island.library_filter.is_empty();
        let search = text_field(
            "library-filter",
            &island.library_filter,
            island.library_editing(),
            // The tab's own noun, as the other search fields name theirs.
            match kind {
                super::catalog::Kind::Props => "Search props (e.g. tree, bush, rock)…",
                super::catalog::Kind::Decorations => "Search decorations…",
                super::catalog::Kind::Objects => "Search objects…",
                super::catalog::Kind::Mod => "Search this mod’s models…",
            },
        )
        .w_full()
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            if let Some(i) = &mut this.island {
                i.edit_library_filter();
            }
            cx.notify();
        }));
        let filled = !island.library_filter.is_empty();
        let search = div()
            .relative()
            .w(px(320.))
            .child(search.when(filled, |d| d.pr(px(22.))))
            .when(filled, |d| {
                d.child(clear_search("library-filter-clear").on_click(cx.listener(
                    |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(i) = &mut this.island {
                            i.library_filter.clear();
                            i.library_edit = None;
                        }
                        cx.notify();
                    },
                )))
            });

        let thumb = super::catalog::THUMB as f32;
        let scatter_mode = island.tool == Tool::Scatter;
        // Cards go into fixed-height rows of a virtualized list: only the
        // rows on screen are built and laid out. A full flex-wrap grid of
        // 300 cards cost ~6 ms of layout and paint per frame.
        struct Card {
            label: SharedString,
            tooltip: String,
            pick: Pick,
            active: bool,
            in_set: bool,
            /// The open island already uses this model.
            here: bool,
            image: Option<Arc<RenderImage>>,
            placeholder: &'static str,
            /// A variant group's size, shown as a badge.
            variants: Option<u32>,
            /// What dragging the card onto a viewport places: the model
            /// itself, or a group's pictured variant.
            drag: Option<Pick>,
        }
        let cards: Vec<Card> = rows
            .into_iter()
            .map(|(label, detail, uses, pick, active, cfg)| {
                let in_set = scatter_mode
                    && matches!(&pick, Pick::Prop(p) if island.in_scatter(&p.replace('\\', "/")));
                Card {
                    variants: matches!(pick, Pick::Group(_))
                        .then(|| detail.split(' ').next()?.parse().ok())
                        .flatten(),
                    tooltip: if kind == super::catalog::Kind::Mod {
                        detail
                    } else if kind == super::catalog::Kind::Props
                        && cfg
                            .as_deref()
                            .is_some_and(|c| here.contains(&c.to_ascii_lowercase()))
                    {
                        format!("{detail} · on this island · {uses}× on shipped islands")
                    } else if matches!(pick, Pick::Group(_)) {
                        // A group's click and drag do different things.
                        format!(
                            "{detail} · {uses}× on shipped islands\nClick to choose a variant · drag to place the pictured one"
                        )
                    } else {
                        format!("{detail} · {uses}× on shipped islands")
                    },
                    // File names, spaced: `tree beech 02 a` wraps and reads.
                    label: pretty(&label).into(),
                    active: active || in_set,
                    in_set,
                    here: kind == super::catalog::Kind::Props
                        && cfg
                            .as_deref()
                            .is_some_and(|c| here.contains(&c.to_ascii_lowercase())),
                    image: cfg.as_deref().and_then(|c| self.thumbs.as_ref()?.get(c)),
                    placeholder: match &cfg {
                        Some(c) if self.thumbs.as_ref().is_some_and(|t| t.missing(c)) => "No preview",
                        Some(_) => "…",
                        None => "no model",
                    },
                    drag: match &pick {
                        Pick::Group(_) => cfg.clone().map(Pick::Prop),
                        other => Some(other.clone()),
                    },
                    pick,
                }
            })
            .collect();
        let cards = std::rc::Rc::new(cards);
        let entity = cx.entity().downgrade();
        let card_query = island.library_filter.clone();
        let grid = widgets::card_grid(
            "library-list",
            cards.len(),
            thumb + 8.,
            window,
            cx,
            move |n, _, _| {
                let card = &cards[n];
                let index = n;
                let (cards, entity) = (cards.clone(), entity.clone());
                widgets::thumbnail_card(
                    ("library-card", n),
                    card.image.clone(),
                    thumb,
                    card.label.clone(),
                    &card_query,
                    card.placeholder,
                    card.active,
                )
                .when(card.in_set, |d| {
                    d.child(
                        widgets::badge("✓ set", theme::get().accent)
                            .absolute()
                            .top(px(7.))
                            .right(px(7.)),
                    )
                })
                // Marks what the island already uses, so the order (these
                // first) explains itself.
                // A dot, not a badge: on every leading card a label was a
                // wall of noise. The tooltip and the count line name it.
                .when(card.here && !card.in_set, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top(px(9.))
                            .left(px(9.))
                            .size(px(7.))
                            .rounded_full()
                            .bg(theme::get().accent),
                    )
                })
                .when_some(card.variants, |d, n| {
                    d.child(
                        // A corner chip on the picture; the tooltip says what
                        // a click does.
                        // "›": the card opens a group, not one model.
                        // Top right, clear of the model: at the bottom it
                        // covered trunks and rock bases. Short ("7 ›", the
                        // card's tooltip says "variants"): "7 variants ›"
                        // spanned two thirds of the tile and covered crowns.
                        div()
                            .id(("library-variants", index))
                            .absolute()
                            .top(px(7.))
                            .right(px(7.))
                            .child(widgets::overlay_badge(format!("{n} ›")))
                            .tooltip(Tooltip::text(
                                format!("{n} variants"),
                                "",
                                "Click the card to choose one; dragging places the one pictured",
                            )),
                    )
                })
                .tooltip(Tooltip::text(card.label.clone(), "", card.tooltip.clone()))
                // Drags onto a viewport; a variant group places the variant
                // its picture shows (a click opens the group).
                .when_some(card.drag.clone(), |d, pick| {
                    d.on_drag(
                        LibraryDrag {
                            pick,
                            label: card.label.clone(),
                            image: card.image.clone(),
                        },
                        |drag, _, _, cx| cx.new(|_| drag.clone()),
                    )
                })
                .on_click(move |_, _, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.pick_library(&cards[n].pick);
                        cx.notify();
                    });
                })
                .into_any_element()
            },
        );
        let more = (total > LIBRARY_ROWS).then(|| {
            div()
                .p_1()
                .text_size(px(11.))
                .text_color(theme::get().muted)
                .child(format!(
                    "Showing {LIBRARY_ROWS} of {total} · search to narrow"
                ))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(
                // Inset like the Assets and UI files filters, both sides: a
                // button at the row's end touched the dock's border.
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_1()
                    .pr_2()
                    .child(tabs)
                    .when_some(group.clone().filter(|_| grouping), |d, g| {
                        d.child(
                            widgets::action(
                                format!("← All · {}", pretty(&g)),
                                widgets::Tone::Secondary,
                            )
                            .h(px(26.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(i) = &mut this.island {
                                    i.library_group = None;
                                }
                                cx.notify();
                            })),
                        )
                    })
                    // Hidden on an empty This mod tab: the empty state
                    // under it says why, and a disabled field over nothing
                    // read as a dead control.
                    .when(!empty_mod_tab, |d| d.child(search))
                    // Narrows Props to what the island uses; the dots and
                    // the order say which those are, this hides the rest.
                    .when(kind == super::catalog::Kind::Props && on_island > 0, |d| {
                        let on = island.library_here;
                        // The kit checkbox, as the Hierarchy's and Assets'
                        // only-filters: a framed chip with a box of its own
                        // was a third style for one kind of control. The
                        // cards' blue dot before the count, as its key.
                        d.child(
                            widgets::checkbox("Only on this island", on, "")
                                .flex_shrink_0()
                                .child(
                                    div()
                                        .ml_1()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_size(px(11.))
                                        .text_color(theme::get().muted)
                                        .child(div().size(px(7.)).rounded_full().bg(theme::get().accent))
                                        .child(on_island.to_string()),
                                )
                                .tooltip(Tooltip::text(
                                    "On this island",
                                    "",
                                    "Cards with a blue dot are used on this island and listed first; tick to show only these",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(i) = &mut this.island {
                                        i.library_here = !i.library_here;
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    // The totals at the far side, apart from the filter's
                    // "● 64": side by side "64 272 models" read as one run
                    // of numbers.
                    .child(div().flex_1())
                    .child(
                        // Counts only; how to use the grid is in the
                        // tooltip, so a narrow pane does not clip it.
                        div()
                            .id("library-count")
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            // The cards' own dot, in their colour, as the
                            // count's legend.
                            // The cards' own dot as legend, only where no chip
                            // carries it (the chip is on Props).
                            .when(
                                on_island > 0
                                    && !scatter_mode
                                    && kind != super::catalog::Kind::Props,
                                |d| {
                                    d.child(
                                        div().size(px(7.)).rounded_full().bg(theme::get().accent),
                                    )
                                },
                            )
                            .tooltip(Tooltip::text(
                                "Library",
                                "",
                                if scatter_mode {
                                    "Click props to add them to the scatter set or take them out"
                                } else {
                                    "Click a model to place it; a card marked with variants opens them"
                                },
                            ))
                            .child(if scatter_mode {
                                if kind == super::catalog::Kind::Props {
                                    format!("{} · scatter set", crate::project::plural(total, "prop", "props"))
                                } else {
                                    "Scatter sets are made of props: switch to the Props tab".to_owned()
                                }
                            } else if total == 0 {
                                // Nothing to count: the empty state below says
                                // it, and a zero beside the dimmed search was
                                // noise. The slot stays, so nothing moves.
                                String::new()
                            } else {
                                // The tab already counts the models; once variants
                                // fold, say how many cards that makes.
                                let counts = if !query.is_empty() && models < unfiltered {
                                    // Against the tab's whole list, as the
                                    // Hierarchy's filtered counts read.
                                    format!(
                                        "{} of {}",
                                        crate::project::count(models),
                                        crate::project::plural(unfiltered, "model", "models")
                                    )
                                } else if total == models {
                                    crate::project::plural(models, "model", "models")
                                } else {
                                    format!(
                                        "{} · {}",
                                        crate::project::plural(total, "model", "models"),
                                        crate::project::plural(models, "variant", "variants")
                                    )
                                };
                                // Says why the first cards come first.
                                // On Props the chip names the island count.
                                if on_island > 0 && kind != super::catalog::Kind::Props {
                                    format!("{on_island} on this island first · {counts}")
                                } else {
                                    counts
                                }
                            }),
                    )
                    // Adding a model stays at hand once the mod has some: the
                    // empty state's button went with it, and a second model
                    // meant a trip to Project → Models.
                    .when(kind == super::catalog::Kind::Mod && total > 0, |d| {
                        d.child(
                            // At the pane's right edge; filled as on the Models
                            // page unless the Inspector's Add island is the
                            // screen's one filled button.
                            widgets::action(
                                "Add model from disk…",
                                if island_cta {
                                    widgets::Tone::Secondary
                                } else {
                                    widgets::Tone::Primary
                                },
                            )
                                .flex_shrink_0()
                                .on_click(cx.listener(|this, _, _, cx| this.add_model(cx))),
                        )
                    }),
            )
            .child(
                div()
                    .id("library-list-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(if total == 0 && kind == super::catalog::Kind::Mod {
                        // Get models without leaving the island. No icon and
                        // the formats on Add model's tooltip, as on Models:
                        // in a dock this shallow the full stack touched both
                        // edges.
                        widgets::empty_state("No models in this mod yet")
                            .child(self.model_ways(true, cx))
                            .into_any_element()
                    } else if total == 0 {
                        widgets::empty_state("Nothing matches the search").into_any_element()
                    } else {
                        grid
                    }),
            )
            .children(more)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::IslandKind;

    #[test]
    fn variants_group_with_or_without_a_trailing_word() {
        use super::variant_base;
        assert_eq!(
            variant_base("tree_maritime_pine_01_k"),
            "tree_maritime_pine_01"
        );
        assert_eq!(variant_base("tree_beech_02_a_soft"), "tree_beech_02_soft");
        assert_eq!(variant_base("tree_beech_02_d_soft"), "tree_beech_02_soft");
        assert_eq!(
            variant_base("bush_generic_01_a_marsh"),
            "bush_generic_01_marsh"
        );
        // A plain model and a numbered one stay themselves.
        assert_eq!(variant_base("bush_gorse_01"), "bush_gorse_01");
        assert_eq!(variant_base("reed_03_b"), "reed_03");
    }

    #[test]
    fn island_titles_leave_the_pack_to_the_detail() {
        use super::{island_pack, island_title};
        let base = "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m";
        let dlc = "data/dlc01/provinces/roman/islands/pool/roman_dlc_01_island_small_01/roman_dlc_01_island_small_01.a7m";
        let egypt = "data/dlc03/provinces/egypt/islands/pool/q/egypt_dlc_03_valley_village_01/egypt_dlc_03_valley_village_01.a7m";
        assert_eq!(island_title(base), "Medium 01");
        assert_eq!(island_pack(base), None);
        assert_eq!(island_title(dlc), "Small 01");
        assert_eq!(island_pack(dlc).as_deref(), Some("DLC 1"));
        assert_eq!(island_title(egypt), "Egypt valley village 01");
        assert_eq!(island_pack(egypt).as_deref(), Some("DLC 3"));
    }

    #[test]
    fn island_kind_reads_the_shipped_file_names() {
        let at = |name: &str| format!("data/base/provinces/celtic/islands/{name}.a7m");
        assert_eq!(
            IslandKind::of(&at("celtic_island_medium_01")),
            IslandKind::Playable
        );
        assert_eq!(
            IslandKind::of(&at("celtic_island_deco_08")),
            IslandKind::Decoration
        );
        // Campaign wins over deco: a campaign decoration is not a free one.
        assert_eq!(
            IslandKind::of(&at("celtic_island_campaign_deco_01")),
            IslandKind::Other
        );
        assert_eq!(
            IslandKind::of(&at("celtic_island_third_party_trader_01")),
            IslandKind::Other
        );
        assert!(IslandKind::All.admits(&at("celtic_island_deco_08")));
        assert!(!IslandKind::Playable.admits(&at("celtic_island_deco_08")));
    }
}
