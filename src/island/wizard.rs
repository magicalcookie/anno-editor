// The New island wizard: a modal card over the editor that collects name,
// region, size and starting terrain, then builds the island off the UI
// thread. The island model lives in `new`.
use super::{IslandEditor, Skeleton, Spec, Start, Tool, new, panels::ListingOf, skeletons};
use crate::editor::{Editor, Workspace};
use crate::project::Listing;
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    widgets::{dialog, hint, segmented},
};
use gpui_editor_kit::{texture::Texture, theme};
use std::sync::Arc;

pub struct Wizard {
    name: TextEdit<()>,
    /// The project's first reserved GUID while it is typed here; keys go
    /// to it instead of the name until Enter or Escape.
    guid: Option<TextEdit<()>>,
    skeletons: Vec<Skeleton>,
    region: String,
    /// Index into `skeletons`.
    size: usize,
    start: Start,
    listing: Listing,
    busy: bool,
    /// The starting terrain at the chosen size, redrawn when either changes.
    sketch: Arc<RenderImage>,
}

/// The preview box's side, in logical pixels.
const PREVIEW_BOX: f32 = 260.;

/// Pixels across the preview image: twice the box, so a 2x screen draws it
/// pixel for pixel. At 400 for a 260 px box the beach ring came out
/// stair-stepped and soft.
const SKETCH_PX: u32 = PREVIEW_BOX as u32 * 2;

/// Metres between the preview's grid lines: the game's 32 m terrain chunk,
/// so the grid counts out the map's width.
const GRID_M: u32 = 32;

fn sketch(start: Start, world: u32) -> Arc<RenderImage> {
    let mut rgba = new::start_preview(start, world.max(1) as f32, SKETCH_PX);
    // Faint lines every 32 m, lightening what is under them.
    let cells = (world / GRID_M).max(1);
    let n = SKETCH_PX as usize;
    for i in 1..cells {
        let at = (i * SKETCH_PX / cells) as usize;
        for t in 0..n {
            for (x, z) in [(at, t), (t, at)] {
                let p = &mut rgba[(z * n + x) * 4..][..3];
                for c in p {
                    *c = (*c as u16 + (255 - *c as u16) / 6) as u8;
                }
            }
        }
    }
    Texture::from_rgba(SKETCH_PX, SKETCH_PX, rgba)
        .expect("preview size is valid")
        .render_image()
}

/// The sketch's colours from the sea to the land's highest point, so the
/// tints read as heights and not as vegetation.
fn height_key(top: f32) -> Div {
    const STEPS: usize = 24;
    div()
        .w(px(PREVIEW_BOX))
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .h(px(6.))
                .flex()
                .rounded(px(2.))
                .overflow_hidden()
                .children((0..STEPS).map(|k| {
                    let y = top * k as f32 / (STEPS - 1) as f32;
                    let [r, g, b] = super::new::land_color(y).map(|v| v.clamp(0., 255.) as u32);
                    div().flex_1().h_full().bg(rgb(r << 16 | g << 8 | b))
                })),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                // A legend for the map above: its colours, end to end.
                // Titled: bare numbers under a bar read as a progress bar.
                .child("Height 0\u{a0}m")
                .child(format!("{top:.0}\u{a0}m")),
        )
}

impl Wizard {
    /// The field keys go to, for platform text input: the GUID while it
    /// is open, else the name.
    pub(crate) fn typing(&mut self) -> &mut dyn gpui_editor_kit::field::InputBuffer {
        match &mut self.guid {
            Some(guid) => guid,
            None => &mut self.name,
        }
    }

    fn sizes(&self) -> impl Iterator<Item = (usize, &Skeleton)> {
        self.skeletons
            .iter()
            .enumerate()
            .filter(|(_, s)| s.region == self.region)
    }

    /// The game's home region (`rules.default_region`) first, then the rest
    /// in skeleton order.
    fn regions(&self, rules: &anno_core::game::IslandRules) -> Vec<String> {
        let mut r: Vec<String> = self.skeletons.iter().map(|s| s.region.clone()).collect();
        r.dedup();
        r.sort_by_key(|r| !r.eq_ignore_ascii_case(rules.default_region));
        r
    }

    fn spec(&self) -> Option<Spec> {
        if self.name.text.trim().is_empty() {
            return None;
        }
        Some(Spec {
            name: self.name.text.clone(),
            skeleton: self.skeletons.get(self.size)?.clone(),
            start: self.start,
            listing: self.listing.clone(),
        })
    }

    /// Types `name` into the Name field, as the keys would.
    #[cfg(test)]
    pub(crate) fn set_name(&mut self, name: &str) {
        self.name = TextEdit::new((), name, Filter::Text);
    }

    pub fn listing(&self) -> &Listing {
        &self.listing
    }

    pub fn listing_mut(&mut self) -> &mut Listing {
        &mut self.listing
    }

    /// Redraws `sketch` after the size or the starting terrain changed.
    fn redraw(&mut self) {
        self.sketch = sketch(self.start, self.world());
    }

    /// The smallest map of this region at least `min` metres wide: its
    /// index and width.
    pub(crate) fn first_width_from(&self, min: u32) -> Option<(usize, u32)> {
        self.sizes()
            .map(|(i, s)| (i, s.world[0]))
            .filter(|&(_, w)| w >= min)
            .min_by_key(|&(_, w)| w)
    }

    pub(crate) fn choose_size(&mut self, index: usize) {
        self.size = index;
        self.redraw();
    }

    /// Map side of the chosen size, metres.
    pub fn world(&self) -> u32 {
        self.skeletons.get(self.size).map_or(0, |s| s.world[0])
    }
}

impl Editor {
    /// File → New island…, asking first when the open island has edits
    /// (`Editor::leave`).
    pub fn open_wizard(&mut self, cx: &mut Context<Self>) {
        self.leave(crate::editor::Leave::NewIsland, cx);
    }

    /// Opens the New island wizard. The open island stays until the new
    /// one is made, so cancelling keeps it.
    pub(crate) fn wizard_now(&mut self) {
        let Some(install) = &self.install else {
            return self.message("No install is open", true);
        };
        let skeletons = skeletons(self.module, install, self.assets.as_deref());
        let Some(first) = skeletons.first() else {
            return self.message("The install ships no islands to build on", true);
        };
        // The open island's region first: a modder working on one province
        // most likely wants another island of it. Then the game's home
        // region.
        // Skeleton regions are province folder names (`celtic`); the
        // island's region is the game's name for it (`Celtic`). Prefer the
        // folder in the open island's path, then the name without case.
        let open_region = self.island.as_ref().and_then(|i| {
            let folder = i
                .path
                .split("/provinces/")
                .nth(1)
                .and_then(|rest| rest.split('/').next());
            let name = i.region();
            skeletons
                .iter()
                .map(|s| &s.region)
                .find(|r| Some(r.as_str()) == folder || r.eq_ignore_ascii_case(&name))
                .cloned()
        });
        let region = if let Some(r) = open_region {
            r
        } else if let Some(home) = (skeletons.iter()).find(|s| {
            s.region
                .eq_ignore_ascii_case(self.island_rules().default_region)
        }) {
            home.region.clone()
        } else {
            first.region.clone()
        };
        // Default to a mid-sized island of the region.
        let of_region: Vec<usize> = (0..skeletons.len())
            .filter(|&i| skeletons[i].region == region)
            .collect();
        let size = of_region[of_region.len() / 2];
        let world = skeletons[size].world[0];
        let start = match Start::offered(self.module) {
            [(only, _)] => *only,
            _ => Start::Plateau,
        };
        self.wizard = Some(Wizard {
            // Empty, as the New project name: the kit's fields have no
            // select-all, so a prefilled name got typed onto.
            name: TextEdit::new((), "", Filter::Text),
            guid: None,
            skeletons,
            region,
            size,
            start,
            listing: Listing::new(self.island_rules()),
            busy: false,
            sketch: sketch(start, world),
        });
    }

    /// Keys while the wizard is open: typing edits the name.
    pub fn wizard_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(w) = &mut self.wizard else { return };
        // The GUID field has the keys while open: Enter keeps the range,
        // Escape drops the edit, not the dialog.
        if let Some(guid) = &mut w.guid {
            match event.keystroke.key.as_str() {
                "escape" => w.guid = None,
                "enter" => {
                    self.commit_wizard_guid();
                }
                _ => {
                    if guid.key_down_in(event, cx) == EditEvent::Ignored {
                        return;
                    }
                }
            }
            cx.notify();
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.wizard = None,
            "enter" => self.create_island(cx),
            _ => {
                if w.name.key_down_in(event, cx) == EditEvent::Ignored {
                    return;
                }
            }
        }
        cx.notify();
    }

    /// Keeps a typed GUID for the project; false, with the reason said,
    /// when it is not a range start. Enter, the Name field and Create all
    /// commit it, so a pasted range is not lost by clicking on.
    fn commit_wizard_guid(&mut self) -> bool {
        let Some(text) = (self.wizard.as_ref())
            .and_then(|w| w.guid.as_ref())
            .map(|g| g.text.clone())
        else {
            return true;
        };
        if text.trim().is_empty() {
            if let Some(w) = &mut self.wizard {
                w.guid = None;
            }
            return true;
        }
        match crate::project::parse_guid_base(&text) {
            Some(base) => {
                if let Some(w) = &mut self.wizard {
                    w.guid = None;
                }
                self.project.set_guid_base(base);
                self.finish_project_edit(format!("Reserved GUIDs start at {base}"));
                true
            }
            None => {
                self.message(
                    "The first GUID of your reserved range, a number like 1938924000",
                    true,
                );
                false
            }
        }
    }

    pub(crate) fn create_island(&mut self, cx: &mut Context<Self>) {
        if !self.commit_wizard_guid() {
            return;
        }
        let (Some(w), Some(install)) = (&mut self.wizard, self.install.clone()) else {
            return;
        };
        let Some(spec) = w.spec().filter(|_| !w.busy) else {
            return;
        };
        w.busy = true;
        self.message(
            format!(
                "Creating {}…",
                (self.module.islands())
                    .map(|i| new::island_path(i, &spec.skeleton.region, &spec.name))
                    .unwrap_or_default()
            ),
            false,
        );
        let module = self.module;
        cx.spawn(async move |this, cx| {
            let created = cx
                .background_executor()
                .spawn(async move { IslandEditor::create(module, &install, &spec) })
                .await;
            this.update(cx, |this, cx| {
                match created {
                    Ok(mut island) => {
                        // A new island is there to be shaped.
                        island.tool = Tool::Raise;
                        this.message(format!("Created {}", island.path), false);
                        this.wizard = None;
                        this.island = Some(island);
                        this.switch(Workspace::Island);
                    }
                    Err(e) => {
                        if let Some(w) = &mut this.wizard {
                            w.busy = false;
                        }
                        this.message(format!("Could not create the island: {e:#}"), true);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn render_wizard(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let w = self.wizard.as_ref()?;
        let regions = segmented(
            "wizard-region",
            w.regions(self.island_rules())
                .into_iter()
                .map(|r| (r.clone(), self.island_rules().province(&r).into())),
            &w.region,
            cx,
            |this, region, _, _| {
                if let Some(w) = &mut this.wizard {
                    w.region = region;
                    let mid = w.sizes().nth(w.sizes().count() / 2).map(|(i, _)| i);
                    if let Some(i) = mid {
                        w.size = i;
                    }
                    w.redraw();
                }
            },
        );
        let sizes = segmented(
            "wizard-size",
            w.sizes()
                // The unit once, in the label: Latium's fourteen widths
                // with "m" on each ran under the preview.
                .map(|(i, s)| (i, s.world[0].to_string().into())),
            &w.size,
            cx,
            |this, i, _, _| {
                if let Some(w) = &mut this.wizard {
                    w.size = i;
                    w.redraw();
                }
            },
        );
        let starts = segmented(
            "wizard-start",
            (Start::offered(self.module).iter()).map(|&(start, name)| (start, name.into())),
            &w.start,
            cx,
            |this, start, _, _| {
                if let Some(w) = &mut this.wizard {
                    w.start = start;
                    w.redraw();
                }
            },
        );
        let path = w
            .skeletons
            .get(w.size)
            .zip(self.module.islands())
            .map(|(s, i)| new::island_path(i, &s.region, &w.name.text))
            .unwrap_or_default();
        let size_m = w.skeletons.get(w.size).map_or(0, |s| s.world[0]);
        let named = !w.name.text.trim().is_empty();
        let choices = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            // As the New project dialog marks it.
            .child(hint("Name · required"))
            .child(
                text_field(
                    "wizard-name",
                    &w.name.text,
                    w.guid.is_none().then(|| w.name.display()),
                    "e.g. Hidden Cove",
                )
                // Back to the name, keeping a typed GUID (an invalid one
                // stays open to fix).
                .on_click(cx.listener(|this, _, _, cx| {
                    this.commit_wizard_guid();
                    cx.notify();
                })),
            )
            // The name is the island's; what follows makes the map, under
            // a heading as Game placement heads its part, so the form does
            // not seem to start halfway down.
            .child(gpui_editor_kit::widgets::section("Map"))
            .child(hint("Region"))
            .child(regions)
            .child(hint("Map width (m)"))
            .child(sizes)
            .child(hint(
                "Sets the map’s edge. The island’s size class follows the land you sculpt.",
            ))
            .child(hint("Starting terrain"))
            .child(starts)
            // What the choice makes, under it, as Role's note sits under Role.
            .child(hint(match w.start {
                Start::Sea => "Open water to sculpt or import into.",
                Start::Plateau => "A low rounded island with a beach.",
                Start::Hills => "The plateau with rolling rises.",
                Start::Copy => "The shipped island of this size as it is: its ground, objects, lakes and coast lines.",
            }))
            .child(gpui_editor_kit::widgets::section("Game placement"))
            .child(self.listing_controls(ListingOf::Wizard, cx));
        // Wide enough for the eight sizes in one row beside the preview.
        let card = dialog("New island")
            .w(px(820.))
            .gap_2()
            // No heading over the name: under the dialog's "New island"
            // title an "Island" caption only repeated it; Map and Game
            // placement head the parts after it. The choices on the left, what they make on the
            // right.
            .child(div().flex().gap_6().child(choices).child(
                preview(size_m, w.sketch.clone())
                    // What the starting land amounts to, from the same terrain
                    // the sketch draws: the size class the game will give it
                    // and how high it rises, so Plateau and Hills read in
                    // metres and not only in shading.
                    // Two short facts, each number with its unit on one line.
                    .children(match super::new::start_relief(w.start, size_m as f32) {
                        None if w.start == Start::Copy => vec![hint(format!(
                            "Copies {}",
                            w.skeletons.get(w.size).map_or("", |s| s.path.as_str())
                        ))],
                        None => vec![hint("No land yet: open water to sculpt or import into.")],
                        Some((side, top)) => vec![
                            height_key(top),
                            hint(format!(
                                "Starting land: about {side}\u{a0}m across · {}",
                                (self.module.islands())
                                    .map_or("", |i| i.rules().size_class(side))
                            )),

                            // What the land still needs once made, as the
                            // next step beside what it is: the placement
                            // checkboxes read as a finished, playable island.
                            // Past a rule: a to-do among the numbers read as
                            // one more fact.
                            hint("Next: add a harbour so players can settle.")
                                .mt_1()
                                .pt_2()
                                .border_t_1()
                                .border_color(theme::get().border),
                        ],
                    })
                    // The Overview's GUID warning, where an island is made:
                    // publishing islands numbered in a range nobody reserved
                    // collides with other mods.
                    // A game without packaging numbers no assets.
                    .when_some(
                        (self.project.guid_base().zip(self.module.packaging()))
                            .filter(|_| !self.project.guid_reserved()),
                        |d, (base, packaging)| {
                        d.child(
                            // Full width with a shrinkable text, or the line
                            // runs out of the dialog unwrapped.
                            div()
                                .w_full()
                                .pt_2()
                                .flex()
                                .items_start()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .mt(px(5.))
                                        .size(px(7.))
                                        .flex_shrink_0()
                                        .rounded_full()
                                        // A grey ring, as on the Overview: an
                                        // open step, nothing wrong yet. Amber
                                        // is for what blocks playing.
                                        .border_1()
                                        .border_color(theme::get().muted),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        // One line while it can wait; "Set
                                        // range…" opens the registry link and
                                        // the field. Six lines of it crowded
                                        // out making the island.
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap_1()
                                                .child(hint(
                                                    "GUIDs aren’t reserved yet. Fine for testing; reserve before publishing.",
                                                ))
                                                .when(w.guid.is_none(), |d| {
                                                    d.child(
                                                        div()
                                                            .id("wizard-guid-open")
                                                            .text_size(px(11.))
                                                            .text_color(theme::get().accent)
                                                            .cursor_pointer()
                                                            .hover(|s| s.underline())
                                                            .child("Reserve or enter GUID range…")
                                                            .on_click(cx.listener(|this, _, _, cx| {
                                                                if let Some(w) = &mut this.wizard {
                                                                    w.guid = Some(TextEdit::new(
                                                                        (),
                                                                        "",
                                                                        Filter::Number,
                                                                    ));
                                                                }
                                                                cx.notify();
                                                            })),
                                                    )
                                                }),
                                        )
                                        .when(w.guid.is_some(), |d| {
                                            d.child(
                                                div()
                                                    .id("wizard-guid-registry")
                                                    .text_size(px(11.))
                                                    .text_color(theme::get().accent)
                                                    .cursor_pointer()
                                                    .hover(|s| s.underline())
                                                    .child("Reserve GUIDs ↗")
                                                    .on_click(move |_, _, cx| {
                                                        cx.open_url(packaging.guid_range.registry)
                                                    }),
                                            )
                                            // The range typed here, so reserving
                                            // does not mean leaving the dialog.
                                            .child(
                                                text_field(
                                                    "wizard-guid",
                                                    w.guid.as_ref().map_or("", |g| g.text.as_str()),
                                                    w.guid.as_ref().map(|g| g.display()),
                                                    "First GUID",
                                                )
                                                .w_full(),
                                            )
                                            .child(hint(format!(
                                                "The first of your reserved range; kept for the whole project. Until then, GUIDs start at {base}.",
                                            )))
                                        })
                                ),
                        )
                    }),
            ))
            // Where it lands, as a quiet footer beside the buttons.
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .mt_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme::get().border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            // The hints' size, as the New project dialog's:
                            // at 10 px the one line saying why Create waits
                            // was the faintest text in the app.
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            .child(if named {
                                format!("Will save to {path}")
                            } else {
                                "Enter a name to create the island".to_owned()
                            }),
                    )
                    .child(
                        gpui_editor_kit::widgets::action(
                            "Cancel",
                            gpui_editor_kit::widgets::Tone::Secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.wizard = None;
                            cx.notify();
                        })),
                    )
                    .child(if named {
                        gpui_editor_kit::widgets::action(
                            if w.busy {
                                "Creating…"
                            } else {
                                "Create island"
                            },
                            gpui_editor_kit::widgets::Tone::Primary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.create_island(cx);
                            cx.notify();
                        }))
                    } else {
                        gpui_editor_kit::widgets::action_off("Create island", "Enter a name first")
                    }),
            );
        Some(gpui_editor_kit::widgets::modal("new-island", card))
    }
}

/// The starting terrain itself (`new::start_preview`) across the whole
/// box, so its relief shows at any size; how the size compares with the
/// region's largest map is a scale bar under it.
fn preview(size_m: u32, sketch: Arc<RenderImage>) -> Div {
    const BOX: f32 = PREVIEW_BOX;
    // Down to the Name label's line, so both columns start together.
    div()
        .w(px(BOX))
        .mt(px(6.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .size(px(BOX))
                .rounded(px(4.))
                .border_1()
                .border_color(theme::get().border)
                .overflow_hidden()
                .child(img(sketch).size_full()),
        )
        // Size in words, which the grid counts. The region's largest is the
        // Map width row's last choice already.
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(format!("{size_m} × {size_m} m · {GRID_M} m grid")),
        )
}
