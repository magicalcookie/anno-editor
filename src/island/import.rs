// The Import wizard: picks an island of the other installed game, maps its
// props and ground onto a province of this one, and builds the result as a
// new island. The conversion itself is `convert`.
use super::{
    IslandEditor, Skeleton, Spec, Start,
    catalog::{Catalog, Thumbs},
    convert::{self, Options, PropRow, SlotRow},
    harbour, islands, port, skeletons,
};
use crate::editor::{Editor, Workspace};
use anno_core::game::{IslandRules, Module};
use anno_core::island::{Level, Problem};
use anno_formats::{assets::Assets, vfs::Install};
use anyhow::Context as _;
use gpui_editor_kit::{
    field::{self, EditEvent, Filter, TextEdit},
    gpui::{prelude::*, *},
    theme,
    widgets::{Tooltip, button, checkbox, hint, vscroll},
};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Import {
    /// The game the island comes from: another, or this one for a region
    /// conversion.
    from: &'static dyn Module,
    /// The other game's install and assets, once open.
    other: Option<(Arc<Install>, Arc<Assets>)>,
    /// Its islands, by path.
    islands: Vec<Skeleton>,
    filter: TextEdit<()>,
    name: TextEdit<()>,
    /// The prop picker's search.
    search: TextEdit<()>,
    typing: Typing,
    /// The prop row whose replacement is being picked.
    picking: Option<usize>,
    /// Thumbnails of the other game's props, rendered from its install.
    pub(super) source_thumbs: Option<Thumbs>,
    picked: Option<String>,
    /// The picked island, loaded with its assets.
    source: Option<Box<IslandEditor>>,
    /// This game's island sizes, the targets.
    skeletons: Vec<Skeleton>,
    region: String,
    rows: Vec<PropRow>,
    /// Replacements picked by hand, by source model; a re-match keeps them.
    chosen: HashMap<String, Option<String>>,
    /// Whether `rows` were matched with every look in; until then names
    /// stand in and the rows match again as the looks arrive.
    matched: bool,
    slots: Vec<SlotRow>,
    /// Each province's slot and harbour records, once read; the slot table
    /// offers what the chosen one can place.
    libraries: HashMap<String, Arc<harbour::Library>>,
    loading_library: Option<String>,
    tab: Tab,
    /// The target's ground: per skeleton island, its material set read with
    /// textures, once loaded (`import_set`).
    sets: HashMap<String, Arc<TargetSet>>,
    loading_set: Option<String>,
    /// The Ground tab: each source material and what it becomes. Hand picks are
    /// kept by source file, so they survive a rematch.
    ground: Vec<convert::GroundRow>,
    ground_chosen: HashMap<String, convert::GroundPick>,
    picking_ground: Option<usize>,
    /// Ground tab mode. On, the island keeps the source's ground in a material
    /// set of its own and every row is brought unless picked otherwise. Off,
    /// the ground is matched to the target set's materials.
    own_ground: bool,
    options: Options,
    /// Look matches below this confidence bring the source game's model
    /// (`convert::BRING_BELOW` until the slider moves it).
    bring_below: f32,
    /// What `rows` were matched at; the slider rematches on release when
    /// it moved off this.
    matched_below: f32,
    /// What is running in the background, if anything.
    busy: Option<String>,
    /// The converted island, held back until the user accepts the losses
    /// the loss policy asks about.
    review: Option<Review>,
}

/// A converted island waiting on its losses to be accepted: each question
/// is what `anno_core::island::asks` groups (all shape losses at once, each
/// gameplay category on its own), with whether it has been accepted.
struct Review {
    island: Box<IslandEditor>,
    summary: String,
    asks: Vec<(Vec<Problem>, bool)>,
}

/// The rules of `module`, an import's source or target: both have islands.
fn rules_of(module: &dyn Module) -> &'static IslandRules {
    (module.islands())
        .expect("an import's games have islands")
        .rules()
}

impl Import {
    /// The field keys go to, for platform text input.
    pub(crate) fn typing(&mut self) -> &mut dyn gpui_editor_kit::field::InputBuffer {
        match self.typing {
            Typing::Filter => &mut self.filter,
            Typing::Name => &mut self.name,
            Typing::Search => &mut self.search,
        }
    }
}

/// What the mapping area shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Props,
    Ground,
    Slots,
}

/// A target material set as the Ground tab offers it.
pub(super) struct TargetSet {
    /// The set file the skeleton island names.
    set: String,
    palette: Arc<crate::terrain::Palette>,
    thumbs: Vec<(u8, Arc<RenderImage>)>,
}

impl TargetSet {
    /// Ids the set does not use; brought materials take them (32 at most).
    fn free(&self) -> usize {
        32 - self.palette.ground_layers.len().min(32)
    }
}

/// Which text field takes the keys.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Typing {
    Filter,
    Name,
    Search,
}

/// Models offered in the picker at most; the search narrows the rest.
const PICKER_TILES: usize = 120;

impl Import {
    /// What the picker offers for `row`: with no search, every model of
    /// the row's role (this region's first); else every prop whose path
    /// has all the words, most placed first.
    fn offers(&self, catalog: &Catalog, rules: &IslandRules, row: &PropRow) -> Vec<String> {
        let words: Vec<String> = (self.search.text.split_whitespace())
            .map(str::to_ascii_lowercase)
            .collect();
        let mut out: Vec<String> = (row.choices.iter())
            .filter(|c| !port::is_ported(c))
            .cloned()
            .collect();
        let mut more: Vec<(u8, u32, &str)> = catalog
            .props
            .iter()
            .filter(|e| {
                let p = e.path.to_ascii_lowercase();
                if words.is_empty() {
                    row.role.is_some() && convert::prop_role(&e.path) == row.role
                } else {
                    words.iter().all(|w| p.contains(w.as_str()))
                }
            })
            .map(|e| {
                let fit = convert::fit(rules, &self.region, &e.path).unwrap_or(2);
                (fit, e.uses, e.path.as_str())
            })
            .collect();
        more.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        if !words.is_empty() {
            out.retain(|c| {
                words
                    .iter()
                    .all(|w| c.to_ascii_lowercase().contains(w.as_str()))
            });
        }
        for (_, _, path) in more {
            if out.len() >= PICKER_TILES {
                break;
            }
            if !out.iter().any(|c| c == path) {
                out.push(path.to_owned());
            }
        }
        out
    }

    /// Rows still matched by name while the props' looks are all in.
    pub(super) fn needs_match(&self) -> bool {
        !self.matched && self.source.is_some()
    }

    fn target(&self) -> Option<&Skeleton> {
        let size = self.source.as_ref()?.world()[0];
        convert::pick_size(&self.skeletons, &self.region, size)
    }

    fn regions(&self) -> Vec<String> {
        let mut r: Vec<String> = self.skeletons.iter().map(|s| s.region.clone()).collect();
        r.dedup();
        r
    }

    fn shown(&self) -> impl Iterator<Item = &Skeleton> {
        let words: Vec<String> = (self.filter.text.split_whitespace())
            .map(str::to_ascii_lowercase)
            .collect();
        self.islands.iter().filter(move |s| {
            let p = s.path.to_ascii_lowercase();
            words.iter().all(|w| p.contains(w.as_str()))
        })
    }
}

/// A thumbnail `size` px square; `…` until it renders.
fn thumb(image: Option<Arc<RenderImage>>, size: f32) -> Div {
    let d = square(size);
    match image {
        Some(image) => d.child(img(image).size(px(size))),
        None => d.child("…"),
    }
}

fn square(size: f32) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .bg(theme::get().bg)
        .text_size(px(9.))
        .text_color(theme::get().muted)
}

/// A picker's tile: `picture` over `name`, marked when `current`; dimmed
/// and inert unless `enabled`.
fn picker_tile(
    id: String,
    picture: Div,
    name: String,
    current: bool,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .w(px(84.))
        .p_1()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .rounded_sm()
        .when(current, |d| d.bg(theme::get().selected))
        .when(!enabled, |d| d.opacity(0.4))
        .when(enabled, |d| {
            d.cursor_pointer().hover(|d| d.bg(theme::get().hover))
        })
        .child(picture)
        .child(div().w_full().text_size(px(10.)).truncate().child(name))
}

fn stem(path: &str) -> &str {
    crate::project::island_name(path)
}

/// The province of this game, by `rules`, closest in climate to region
/// `from` of the other.
fn suggest_region(rules: &IslandRules, from: &str, regions: &[String]) -> Option<String> {
    let want = (rules.import_regions.iter()).find_map(|&(f, to)| (f == from).then_some(to))?;
    regions.iter().find(|r| r.as_str() == want).cloned()
}

impl Editor {
    /// Import island…: from the first game Import reads that is installed
    /// here, else the first, which then says it isn't. The dialog switches
    /// between them.
    pub fn open_import(&mut self, cx: &mut Context<Self>) {
        let sources = self.import_sources();
        let from = (sources.iter())
            .find(|m| crate::launcher::install_dir(m.game()).is_some())
            .or(sources.first());
        if let Some(&from) = from {
            self.open_import_from(from, cx);
        }
    }

    /// File → Convert island to another region…: the import wizard with
    /// this game as the source, the open island picked.
    pub fn open_convert(&mut self, cx: &mut Context<Self>) {
        self.open_import_from(self.module, cx);
    }

    /// The import wizard reading islands from `from`: another game, or
    /// this one for a region conversion.
    pub(crate) fn open_import_from(&mut self, from: &'static dyn Module, cx: &mut Context<Self>) {
        let (Some(_), Some(assets)) = (&self.install, &self.assets) else {
            return self.message("The game is still loading; try again in a moment", true);
        };
        if self.island.as_ref().is_some_and(|i| i.dirty()) {
            return self.message("Save the open island (Ctrl S) before importing one", true);
        }
        let same = from.game() == self.game;
        let dir = match crate::launcher::install_dir(from.game()) {
            Some(dir) => dir,
            None if same => Default::default(),
            None => {
                return self.message(
                    format!(
                        "{} is not installed here, or its folder is unknown: open it once from the game picker",
                        from.profile().title
                    ),
                    true,
                );
            }
        };
        // Within one game the open install, mod included, is the source: a
        // project island converts as well as a shipped one.
        let here = same.then(|| (self.install.clone().expect("checked"), assets.clone()));
        let open = (same.then(|| self.island.as_ref().map(|i| i.path.clone()))).flatten();
        let skeletons = skeletons(
            self.module,
            self.install.as_ref().expect("checked"),
            Some(assets),
        );
        let region = skeletons
            .first()
            .map(|s| s.region.clone())
            .unwrap_or_default();
        self.import = Some(Import {
            from,
            other: None,
            islands: Vec::new(),
            filter: TextEdit::new((), "", Filter::Text),
            name: TextEdit::new((), "", Filter::Text),
            search: TextEdit::new((), "", Filter::Text),
            typing: Typing::Filter,
            picking: None,
            source_thumbs: None,
            chosen: HashMap::new(),
            matched: false,
            picked: None,
            source: None,
            skeletons,
            region,
            rows: Vec::new(),
            slots: Vec::new(),
            libraries: HashMap::new(),
            loading_library: None,
            tab: Tab::Props,
            sets: HashMap::new(),
            loading_set: None,
            ground: Vec::new(),
            ground_chosen: HashMap::new(),
            picking_ground: None,
            // Within one game the target province's own ground is the point.
            own_ground: !same,
            // Painted tint is the source province's color; a conversion
            // starts from the target's plain ground.
            options: Options {
                tint: !same,
                ..Options::default()
            },
            // A region conversion swaps to the province's own models unless
            // asked to keep poor matches; an import brings the source game's.
            bring_below: if same { 0. } else { convert::BRING_BELOW },
            matched_below: if same { 0. } else { convert::BRING_BELOW },
            busy: Some(format!("Opening {}…", from.profile().title)),
            review: None,
        });
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    let (install, assets) = match here {
                        Some(here) => here,
                        None => {
                            let families = from.profile().archive_families;
                            let install = Arc::new(Install::open(&dir, families)?);
                            let assets = Arc::new(crate::games::assets(from, &install)?);
                            (install, assets)
                        }
                    };
                    let list = islands(from, &install, Some(&assets));
                    anyhow::Ok((install, assets, list))
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(w) = &mut this.import else { return };
                match opened {
                    Ok((install, assets, list)) => {
                        w.other = Some((install, assets));
                        w.islands = list;
                        w.busy = None;
                        if let Some(path) = open {
                            this.import_pick(path, cx);
                        }
                    }
                    Err(e) => {
                        this.import = None;
                        this.message(
                            format!("Could not open {}: {e:#}", from.profile().title),
                            true,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Typing filters the island list, edits the name once clicked, or
    /// searches the prop picker. Escape closes the picker, then the dialog.
    pub fn import_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(w) = &mut self.import else { return };
        if event.keystroke.key == "escape" {
            if w.picking.take().is_some() || w.picking_ground.take().is_some() {
                w.typing = Typing::Filter;
            } else {
                self.import = None;
            }
        } else {
            let edit = match w.typing {
                Typing::Filter => &mut w.filter,
                Typing::Name => &mut w.name,
                Typing::Search => &mut w.search,
            };
            if edit.key_down_in(event, cx) == EditEvent::Ignored {
                return;
            }
        }
        cx.notify();
    }

    /// Sets what the Ground row being picked becomes, kept over rematches.
    fn import_choose_ground(&mut self, pick: convert::GroundPick) {
        let Some(w) = &mut self.import else { return };
        let Some(r) = w.picking_ground.and_then(|i| w.ground.get_mut(i)) else {
            return;
        };
        r.pick = pick;
        w.ground_chosen.insert(r.file.clone(), pick);
        w.picking_ground = None;
    }

    /// Sets the picked row's replacement; `None` leaves it out.
    fn import_choose(&mut self, target: Option<String>) {
        let Some(w) = &mut self.import else { return };
        let Some(r) = w.picking.and_then(|i| w.rows.get_mut(i)) else {
            return;
        };
        w.chosen.insert(r.source.clone(), target.clone());
        r.pick = target.map(|t| match r.choices.iter().position(|c| *c == t) {
            Some(k) => k,
            None => {
                r.choices.push(t);
                r.choices.len() - 1
            }
        });
        let look = |t: &Option<Thumbs>, cfg: &str| t.as_ref().and_then(|t| t.look(cfg));
        r.scale = convert::height_scale(
            look(&w.source_thumbs, &r.source),
            r.target().and_then(|t| look(&self.thumbs, t)),
        );
        w.picking = None;
        w.typing = Typing::Filter;
    }

    /// Loads the picked island, and this game's catalog for the mapping.
    fn import_pick(&mut self, path: String, cx: &mut Context<Self>) {
        let (Some(target), catalog) = (self.install.clone(), self.catalog.clone()) else {
            return;
        };
        let Some(w) = &mut self.import else { return };
        let Some((install, assets)) = w.other.clone() else {
            return;
        };
        if w.busy.is_some() {
            return;
        }
        w.picked = Some(path.clone());
        let same = w.from.game() == self.game;
        // As the source's island list groups it: 2070's islands have no
        // region of their own, only their folder.
        let from_region = (w.islands.iter())
            .find(|s| s.path == path)
            .map(|s| s.region.clone())
            .unwrap_or_else(|| rules_of(w.from).island_region(&path).to_ascii_lowercase());
        let suggested = if same {
            // Any province but its own; the list keeps the game's order.
            w.regions().into_iter().find(|r| *r != from_region)
        } else {
            suggest_region(rules_of(self.module), &from_region, &w.regions())
        };
        if let Some(region) = suggested {
            w.region = region;
        }
        w.source = None;
        w.rows.clear();
        w.chosen.clear();
        w.name.text = if same {
            format!("{} {}", stem(&path), w.region)
        } else {
            format!("{} {}", stem(&path), w.from.profile().short_name)
        };
        w.busy = Some(format!("Reading {}…", stem(&path)));
        let (from, module, target_assets) = (w.from, self.module, self.assets.clone());
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let mut src = IslandEditor::load(from, &install, &path)?;
                    src.assets = Some(assets);
                    let catalog = match catalog {
                        Some(c) => c,
                        None => Arc::new(Catalog::of(module, &target, target_assets.as_deref())?),
                    };
                    anyhow::Ok((Box::new(src), catalog))
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(w) = &mut this.import else { return };
                w.busy = None;
                match loaded {
                    Ok((src, catalog)) => {
                        w.source = Some(src);
                        if w.source_thumbs.is_none()
                            && let Some((install, _)) = &w.other
                        {
                            // The other game's own thumbnail folder: the
                            // Library there fills and reuses the same cache.
                            let dir = gpui_editor_kit::platform::config_dir(
                                w.from.profile().config_folder,
                            )
                            .join("thumbnails");
                            let look = w.from.model_look();
                            w.source_thumbs = Some(Thumbs::new(install.clone(), dir, look));
                        }
                        this.catalog.get_or_insert(catalog);
                        this.import_remap();
                        this.import_library(cx);
                        this.import_set(cx);
                    }
                    Err(e) => {
                        w.picked = None;
                        this.message(format!("Could not read the island: {e:#}"), true);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Rematches once the "Import below" slider is let go off the value
    /// the rows were matched at.
    fn import_rematch_if_moved(&mut self) {
        if self
            .import
            .as_ref()
            .is_some_and(|w| w.bring_below != w.matched_below)
        {
            self.import_remap();
        }
    }

    /// A fresh prop mapping for the picked island and province.
    pub(super) fn import_remap(&mut self) {
        // Ask for every look the match needs; cached ones arrive within a
        // poll, so `pending` below is true until all are in.
        self.start_thumbs();
        if let (Some(t), Some(catalog)) = (&mut self.thumbs, &self.catalog) {
            t.request(convert::match_pool(catalog));
        }
        if let Some(w) = &mut self.import
            && let (Some(t), Some(src)) = (&mut w.source_thumbs, &w.source)
        {
            let models = convert::source_models(src);
            t.request(models.iter().map(|m| m.0.as_str()));
        }
        let (Some(w), Some(catalog)) = (&mut self.import, &self.catalog) else {
            return;
        };
        let Some(src) = &w.source else { return };
        let look = |t: &Option<Thumbs>, cfg: &str| t.as_ref().and_then(|t| t.look(cfg));
        let pending = |t: &Option<Thumbs>| t.as_ref().is_none_or(|t| t.pending());
        w.matched = !pending(&w.source_thumbs) && !pending(&self.thumbs);
        w.matched_below = w.bring_below;
        w.rows = convert::map_props(
            &convert::source_models(src),
            catalog,
            (w.from, self.module),
            &w.region,
            w.bring_below,
            |cfg| look(&w.source_thumbs, cfg),
            |cfg| look(&self.thumbs, cfg),
        );
        for r in &mut w.rows {
            let Some(target) = w.chosen.get(&r.source) else {
                continue;
            };
            r.pick = target
                .as_ref()
                .map(|t| match r.choices.iter().position(|c| c == t) {
                    Some(k) => k,
                    None => {
                        r.choices.push(t.clone());
                        r.choices.len() - 1
                    }
                });
            r.scale = convert::height_scale(
                look(&w.source_thumbs, &r.source),
                r.target().and_then(|t| look(&self.thumbs, t)),
            );
        }
        w.ground = match w.target().and_then(|s| w.sets.get(&s.path)) {
            Some(t) => {
                let mut rows =
                    convert::ground_rows(src, &t.palette, &rules_of(self.module).ground_names);
                for r in &mut rows {
                    if w.own_ground {
                        r.pick = convert::GroundPick::Bring;
                    }
                    if let Some(&pick) = w.ground_chosen.get(&r.file) {
                        r.pick = pick;
                    }
                }
                rows
            }
            None => Vec::new(),
        };
        w.slots = match w.libraries.get(&w.region) {
            Some(lib) => convert::map_slots(src, lib),
            None => Vec::new(),
        };
    }

    /// Reads the chosen province's slot records once (a few islands' object
    /// lists, well under a second), then maps the slots.
    fn import_library(&mut self, cx: &mut Context<Self>) {
        let (Some(w), Some(install), Some(assets)) =
            (&mut self.import, self.install.clone(), self.assets.clone())
        else {
            return;
        };
        if w.libraries.contains_key(&w.region) || w.loading_library.is_some() {
            return;
        }
        // The game's spelling of the region (`Roman`, `Moderate`), from a
        // shipped island path.
        let Some(game_region) = (w.skeletons.iter())
            .find(|s| s.region == w.region)
            .map(|s| rules_of(self.module).island_region(&s.path).to_owned())
        else {
            return;
        };
        let region = w.region.clone();
        w.loading_library = Some(region.clone());
        let module = self.module;
        cx.spawn(async move |this, cx| {
            let lib = cx
                .background_executor()
                .spawn(
                    async move { harbour::Library::load(module, &install, &assets, &game_region) },
                )
                .await;
            this.update(cx, |this, cx| {
                let Some(w) = &mut this.import else { return };
                w.loading_library = None;
                match lib {
                    Ok(lib) => {
                        w.libraries.insert(region, Arc::new(lib));
                        this.import_remap();
                    }
                    Err(e) => this.message(format!("Slot records unavailable: {e:#}"), true),
                }
                // The province may have changed while this one loaded.
                this.import_library(cx);
                this.import_set(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads the target skeleton's material set (with its textures, for the
    /// Ground tab's thumbnails and matching) in the background.
    fn import_set(&mut self, cx: &mut Context<Self>) {
        let (Some(w), Some(install)) = (&mut self.import, self.install.clone()) else {
            return;
        };
        // Only a7m islands name a material set; conversion maps the source
        // ground onto 2070's own layers.
        let Some(skeleton) = (w.target().map(|s| s.path.clone())).filter(|p| p.ends_with(".a7m"))
        else {
            return;
        };
        if w.sets.contains_key(&skeleton) || w.loading_set.is_some() {
            return;
        }
        let Some(a7m) = self.module.islands().and_then(|i| i.codec().a7m()) else {
            return;
        };
        w.loading_set = Some(skeleton.clone());
        cx.spawn(async move |this, cx| {
            let key = skeleton.clone();
            let read = cx
                .background_executor()
                .spawn(async move {
                    let archive =
                        anno_formats::rda::Archive::parse(install.read(&skeleton)?, &skeleton)?;
                    let named = anno_formats::a7m::island::material_set_of(
                        &archive.read_path("rd3d.data")?,
                    )?;
                    let set = a7m.material_set(named.as_deref()).to_owned();
                    let palette = crate::terrain::Palette::load(&install, None, &set)?;
                    let thumbs = (palette.materials.iter())
                        .map(|(&id, m)| (id, m.preview.render_image()))
                        .collect();
                    anyhow::Ok(TargetSet {
                        set,
                        palette: Arc::new(palette),
                        thumbs,
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(w) = &mut this.import else { return };
                w.loading_set = None;
                match read {
                    Ok(set) => {
                        w.sets.insert(key, Arc::new(set));
                        this.import_remap();
                    }
                    Err(e) => this.message(format!("Ground materials unavailable: {e:#}"), true),
                }
                // The province or size may have changed meanwhile.
                this.import_set(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Builds the new island in the background and opens it.
    fn import_run(&mut self, cx: &mut Context<Self>) {
        let (Some(w), Some(install), Some(assets)) =
            (&mut self.import, self.install.clone(), self.assets.clone())
        else {
            return;
        };
        if w.busy.is_some() {
            return;
        }
        let Some(skeleton) = w.target().cloned() else {
            return;
        };
        let Some(src) = w.source.take() else { return };
        // Converting onto a project island again keeps its listing, and
        // with it the losses already accepted for it.
        let path = super::island_path(
            self.module.islands().expect("an import target has islands"),
            &skeleton.region,
            &w.name.text,
        );
        let listing = (self.project.islands.iter())
            .find(|e| e.path == path)
            .map_or_else(
                || crate::project::Listing::new(rules_of(self.module)),
                |e| e.listing.clone(),
            );
        // A game whose islands can't be blanked converts onto a copy of
        // the template, its props and objects cleared as a blank's are.
        let sea = Start::offered(self.module)
            .iter()
            .any(|&(s, _)| s == Start::Sea);
        let spec = Spec {
            name: w.name.text.clone(),
            skeleton,
            start: if sea { Start::Sea } else { Start::Copy },
            listing,
        };
        let (mut rows, slots, options) = (w.rows.clone(), w.slots.clone(), w.options);
        // Should a port fail, the row takes its best match of this game.
        let look = |t: &Option<Thumbs>, cfg: &str| t.as_ref().and_then(|t| t.look(cfg));
        let fallback: Vec<Option<(usize, f32)>> = (rows.iter())
            .map(|r| {
                let k = r.choices.iter().position(|c| !port::is_ported(c))?;
                let scale = convert::height_scale(
                    look(&w.source_thumbs, &r.source),
                    look(&self.thumbs, &r.choices[k]),
                );
                Some((k, scale))
            })
            .collect();
        let other = w.other.as_ref().map(|o| o.0.clone());
        let lib = w.libraries.get(&w.region).cloned();
        let catalog = self.catalog.clone();
        // Ground rows as picked, and for each material to bring, how its source
        // set tiles and blends it.
        let ground = w.ground.clone();
        let target_set = w.sets.get(&spec.skeleton.path).map(|t| t.set.clone());
        let own_ground = w.own_ground;
        let source_set = src.island.material_set.clone();
        let builds_set = !ground.is_empty()
            && (own_ground || ground.iter().any(|r| r.pick == convert::GroundPick::Bring));
        w.busy = Some("Converting…".into());
        let (from_game, game) = (w.from, self.module);
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let mut dst = IslandEditor::create(game, &install, &spec)?;
                    if spec.start == Start::Copy {
                        dst.island.doc.props.clear();
                        Arc::make_mut(&mut dst.island.doc.objects).clear();
                        // Its codec places objects as copies of shipped
                        // records (2070's), read from the install.
                        dst.install = Some(install.clone());
                    }
                    dst.island.harbour = match lib {
                        Some(lib) => Some(lib),
                        None => harbour::Library::load(game, &install, &assets, &dst.region())
                            .map_err(|e| eprintln!("import: harbour records: {e:#}"))
                            .ok()
                            .map(Arc::new),
                    };
                    dst.assets = Some(assets);
                    let mut ported = Vec::new();
                    // Rows that fell back to their match, and pieces of brought
                    // models that were left out. Reported in the status line,
                    // not only on stderr.
                    let (mut failed, mut left_out) = (Vec::new(), 0);
                    for (r, fallback) in rows.iter_mut().zip(fallback) {
                        if !r.brings() {
                            continue;
                        }
                        let done = (other.as_deref())
                            .context("the other game is not open")
                            .and_then(|from| {
                                port::port(from_game, game, from, &install, &r.source)
                            });
                        match done {
                            Ok(p) => {
                                for why in &p.left_out {
                                    eprintln!("import: {}: left out {why}", r.source);
                                }
                                left_out += p.left_out.len();
                                ported.extend(p.files);
                                r.pieces = p.pieces;
                                r.effects = p.effects;
                            }
                            Err(e) => {
                                eprintln!("import: {}: {e:#}", r.source);
                                failed.push(stem(&r.source).to_owned());
                                (r.pick, r.scale) = match fallback {
                                    Some((k, scale)) => (Some(k), scale),
                                    None => (None, 1.),
                                };
                            }
                        }
                    }
                    // The island's ground set, written into the mod: the
                    // source's own materials, or the target set with brought
                    // ones on top.
                    let mut note = String::new();
                    let mut brought = convert::Brought::default();
                    if builds_set && let (Some(from), Some(set)) = (other.as_deref(), &target_set) {
                        let ground_set = (set.as_str(), source_set.as_str());
                        brought = convert::ground_set(
                            from_game, from, &install, &mut dst, ground_set, &ground, own_ground,
                        )?;
                        ported.append(&mut brought.files);
                        for file in &brought.failed {
                            note += &format!("; ground {} kept its match", stem(file));
                        }
                    }
                    let ground_map =
                        (!ground.is_empty()).then(|| convert::ground_map(&ground, &brought));
                    let effect = catalog
                        .as_deref()
                        .and_then(|c| convert::effect_template(&install, c));
                    let report = dst.convert_from(
                        &src,
                        &rows,
                        &slots,
                        &options,
                        effect.as_ref(),
                        ground_map.as_ref(),
                    );
                    if !failed.is_empty() {
                        let shown: Vec<&str> = failed.iter().take(3).map(String::as_str).collect();
                        note += &format!(
                            "; {} could not be brought over and use their match ({}{})",
                            crate::project::plural(failed.len(), "model", "models"),
                            shown.join(", "),
                            if failed.len() > 3 { ", …" } else { "" }
                        );
                    }
                    if left_out > 0 {
                        note += &format!(
                            "; {} of brought models left out (decals, effects, animated parts)",
                            crate::project::plural(left_out, "piece", "pieces")
                        );
                    }
                    anyhow::Ok((dst, report, src, ported, note))
                })
                .await;
            this.update(cx, |this, cx| {
                match built {
                    Ok((mut island, report, _, ported, note)) => {
                        // Ported models go into the mod now, so the island
                        // shows them before its first save.
                        if !ported.is_empty() {
                            for (path, bytes) in ported {
                                this.project.stage(&path, bytes);
                            }
                            match this.project.write() {
                                Ok(_) => this.mount_project(),
                                Err(e) => {
                                    this.message(format!("Ported models not written: {e:#}"), true)
                                }
                            }
                        }
                        // The island's own line, or its last harbour
                        // message would cover the summary.
                        let summary = format!("Imported: {}{note}", report.summary());
                        let problems = report.problems();
                        let asks: Vec<(Vec<Problem>, bool)> =
                            anno_core::island::asks(&island.listing.accepted, this.game, &problems)
                                .into_iter()
                                .map(|q| (q.into_iter().cloned().collect(), false))
                                .collect();
                        if asks.is_empty() {
                            island.say(summary, false);
                            this.import_open(island);
                        } else if let Some(w) = &mut this.import {
                            w.busy = None;
                            w.review = Some(Review {
                                island: Box::new(island),
                                summary,
                                asks,
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(w) = &mut this.import {
                            w.busy = None;
                            w.picked = None;
                        }
                        this.message(format!("Import failed: {e:#}"), true);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens a converted island and closes the wizard.
    fn import_open(&mut self, island: IslandEditor) {
        self.import = None;
        self.island = Some(island);
        self.switch(Workspace::Island);
    }

    /// Opens the reviewed island once every question is accepted, keeping
    /// the acceptances in its listing.
    fn import_accept_all(&mut self) {
        let game = self.game;
        let Some(review) = self.import.as_mut().and_then(|w| w.review.take()) else {
            return;
        };
        let Review {
            mut island,
            summary,
            asks,
        } = review;
        island.set_listing(|l| {
            for (problems, _) in &asks {
                let problems: Vec<&Problem> = problems.iter().collect();
                anno_core::island::accept(&mut l.accepted, game, &problems);
            }
        });
        island.say(summary, false);
        self.import_open(*island);
    }

    /// The review step: what the conversion loses that the policy asks
    /// about, one Accept per question, then Open island.
    fn render_import_review(&self, review: &Review, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut questions = div().flex().flex_col().gap_2();
        for (k, (problems, accepted)) in review.asks.iter().enumerate() {
            let level = match problems.first().map(|p| p.level) {
                Some(Level::Shape) => "Shape",
                Some(Level::Gameplay) => "Gameplay",
                _ => "Loss",
            };
            let said: Vec<String> = problems.iter().map(Problem::said).collect();
            questions = questions.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .child(format!("{level}: {}", said.join("; "))),
                    )
                    .child(
                        button(if *accepted { "Accepted" } else { "Accept" }, !*accepted).on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Some(r) =
                                    this.import.as_mut().and_then(|w| w.review.as_mut())
                                    && let Some(q) = r.asks.get_mut(k)
                                {
                                    q.1 = true;
                                }
                                cx.notify();
                            }),
                        ),
                    ),
            );
        }
        let all = review.asks.iter().all(|(_, accepted)| *accepted);
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                button("Back", false).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(w) = &mut this.import {
                        w.review = None;
                    }
                    cx.notify();
                })),
            )
            .child(
                button("Open island", all).on_click(cx.listener(move |this, _, _, cx| {
                    if all {
                        this.import_accept_all();
                    }
                    cx.notify();
                })),
            );
        let card = gpui_editor_kit::widgets::dialog("Accept what the conversion loses")
            .w(px(640.))
            .gap_3()
            .child(hint(review.summary.clone()))
            .child(hint(
                "Shape losses are confirmed together, gameplay losses one kind at a time. The island remembers what you accept; converting it again asks only about new losses."
                    .to_owned(),
            ))
            .child(questions)
            .child(footer);
        gpui_editor_kit::widgets::modal("import-review", card)
    }

    pub fn render_import(&mut self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if let Some(review) = self.import.as_ref().and_then(|w| w.review.as_ref()) {
            return Some(self.render_import_review(review, cx));
        }
        // Thumbnails first: the table's two sides, and the picker's tiles.
        let w = self.import.as_ref()?;
        // A region conversion: source and target are this game.
        let same = w.from.game() == self.game;
        let offers = match (w.picking.and_then(|i| w.rows.get(i)), &self.catalog) {
            (Some(row), Some(catalog)) => w.offers(catalog, rules_of(self.module), row),
            _ => Vec::new(),
        };
        let sources: Vec<String> = w.rows.iter().map(|r| r.source.clone()).collect();
        let mut targets: Vec<String> = (w.rows.iter())
            .filter_map(|r| r.target().map(str::to_owned))
            .collect();
        targets.extend(offers.iter().cloned());
        self.start_thumbs();
        if let Some(t) = &mut self.thumbs {
            t.request(targets.iter().map(String::as_str));
        }
        if let Some(t) = self.import.as_mut().and_then(|w| w.source_thumbs.as_mut()) {
            t.request(sources.iter().map(String::as_str));
        }
        self.poll_thumbs(cx);
        let w = self.import.as_ref()?;
        let target_thumb = |cfg: &str| self.thumbs.as_ref().and_then(|t| t.get(cfg));
        let source_thumb = |cfg: &str| w.source_thumbs.as_ref().and_then(|t| t.get(cfg));
        let field = |edit: &TextEdit<()>, focused: bool| {
            div()
                .px_2()
                .py_1()
                .border_1()
                .border_color(if focused {
                    theme::get().highlight
                } else {
                    theme::get().border
                })
                .rounded_sm()
                .text_size(px(12.))
                // The kit draws the caret and selection, and selects with
                // the mouse.
                .child(gpui_editor_kit::field::edit_text(
                    if focused {
                        "import-edit-on"
                    } else {
                        "import-edit"
                    },
                    edit.display(),
                    focused,
                ))
        };

        // Left: the other game's islands.
        let mut list = div().flex().flex_col().w_full();
        for s in w.shown().take(400) {
            let path = s.path.clone();
            let picked = w.picked.as_deref() == Some(&s.path);
            list = list.child(
                div()
                    .id(ElementId::Name(format!("import-{}", s.path).into()))
                    .flex()
                    .gap_2()
                    .px_2()
                    .py(px(3.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(picked, |d| d.bg(theme::get().selected))
                    .hover(|d| d.bg(theme::get().hover))
                    .child(div().flex_1().truncate().child(stem(&s.path).to_owned()))
                    .child(div().text_color(theme::get().muted).child(format!(
                        "{} · {} m",
                        rules_of(self.module).province(&s.region),
                        s.world[0]
                    )))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.import_pick(path.clone(), cx);
                        cx.notify();
                    })),
            );
        }
        let left = div()
            .w(px(380.))
            .flex()
            .flex_col()
            .gap_2()
            .child(hint(if same {
                format!(
                    "Island to convert ({} shipped; type to filter)",
                    w.islands.len()
                )
            } else {
                format!(
                    "Island from {} ({} shipped; type to filter)",
                    w.from.profile().title,
                    w.islands.len()
                )
            }))
            .child(
                div()
                    .id("import-filter")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.typing = Typing::Filter;
                        }
                        cx.notify();
                    }))
                    .child(field(&w.filter, w.typing == Typing::Filter)),
            )
            .child(
                vscroll("import-islands", &self.scrolls.get("import-islands"), list)
                    .flex_1()
                    .min_h_0()
                    .bg(theme::get().input),
            );

        // Right: province, name, what to carry over, the prop mapping.
        let row = || div().flex().flex_wrap().gap_1();
        let mut regions = row();
        for region in w.regions() {
            let r = region.clone();
            regions = regions.child(
                button(rules_of(self.module).province(&region), w.region == region)
                    .text_size(px(12.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.region = r.clone();
                        }
                        this.import_remap();
                        this.import_library(cx);
                        this.import_set(cx);
                        cx.notify();
                    })),
            );
        }
        let size = match (w.source.as_deref(), w.target()) {
            (Some(src), Some(t)) => {
                let from = src.world()[0];
                let how = match t.world[0].cmp(&from) {
                    std::cmp::Ordering::Equal => "same size",
                    std::cmp::Ordering::Greater => "centred, open sea around it",
                    std::cmp::Ordering::Less => "the largest size: the rim is cropped",
                };
                format!("{from} m → {} m ({how})", t.world[0])
            }
            _ => "Pick an island on the left".to_owned(),
        };
        let toggle = |name: &'static str, on: bool, tip: &'static str, f: fn(&mut Options)| {
            checkbox(name, on, "")
                .tooltip(Tooltip::text(name, "", tip))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(w) = &mut this.import {
                        f(&mut w.options);
                    }
                    cx.notify();
                }))
        };
        let o = w.options;
        let options = div()
            .flex()
            .flex_wrap()
            .gap_x_4()
            .child(toggle("Ground materials", o.materials, "Each source material becomes this game’s material of the same kind: grass, dirt, rock, sand, gravel, sea floor", |o| o.materials = !o.materials))
            .child(toggle("Tint", o.tint, "The ground tint, as painted", |o| o.tint = !o.tint))
            .child(toggle("Props", o.props, "Trees, bushes and rocks, as mapped below. Decoration objects become props too", |o| o.props = !o.props))
            .child(toggle("Rivers & lakes", o.water, "Curves and their River/Water cells; the beds are already in the terrain", |o| o.water = !o.water))
            .child(toggle("Harbours", o.harbours, "Coastline curves, from this province’s records", |o| o.harbours = !o.harbours))
            .child(toggle("Slots", o.slots, "Mines and river slots. Kinds the province lacks (clay, oil, marsh…) are left out", |o| o.slots = !o.slots));

        let placed: usize = w.rows.iter().map(|r| r.count).sum();
        let mapped: usize = w
            .rows
            .iter()
            .filter(|r| r.pick.is_some())
            .map(|r| r.count)
            .sum();
        let mut table = div().flex().flex_col().w_full();
        for (i, r) in w.rows.iter().enumerate() {
            let target = match r.target() {
                None => "— left out".to_owned(),
                Some(_) if r.brings() => {
                    format!("→ Anno {} model, brought over", w.from.profile().short_name)
                }
                Some(t) if t == r.source => "→ kept as is".to_owned(),
                Some(t) => format!("→ {}", stem(t)),
            };
            let sure = r
                .confidence
                .map_or_else(String::new, |c| format!("{:.0}%", c * 100.));
            let tip = if r.choices.is_empty() {
                "No match of this kind: click to pick any model".to_owned()
            } else {
                "Click to pick the replacement".to_owned()
            };
            table = table.child(
                div()
                    .id(ElementId::Name(format!("import-row-{i}").into()))
                    .flex()
                    .gap_2()
                    .px_2()
                    .py(px(2.))
                    .items_center()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(w.picking == Some(i), |d| d.bg(theme::get().selected))
                    .hover(|d| d.bg(theme::get().hover))
                    .tooltip(Tooltip::text(r.source.clone(), "", tip))
                    .child(thumb(source_thumb(&r.source), 28.))
                    .child(
                        div()
                            .w(px(160.))
                            .truncate()
                            .child(stem(&r.source).to_owned()),
                    )
                    .child(
                        div()
                            .w(px(46.))
                            .text_color(theme::get().muted)
                            .child(format!("×{}", r.count)),
                    )
                    .child(
                        div()
                            .w(px(96.))
                            .text_color(theme::get().muted)
                            .truncate()
                            .child(r.role.map_or("unknown", |r| r.label())),
                    )
                    .child(
                        div()
                            .w(px(34.))
                            .text_color(match r.confidence {
                                Some(c) if c < w.bring_below => theme::get().highlight,
                                _ => theme::get().muted,
                            })
                            .child(sure),
                    )
                    .child(match r.target() {
                        Some(_) if r.brings() => thumb(source_thumb(&r.source), 28.),
                        Some(t) => thumb(target_thumb(t), 28.),
                        None => div().size(px(28.)).flex_none(),
                    })
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .when(r.pick.is_none(), |d| d.text_color(theme::get().muted))
                            .child(target),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.picking = Some(i);
                            w.typing = Typing::Search;
                            w.search = TextEdit::new((), "", Filter::Text);
                        }
                        cx.notify();
                    })),
            );
        }
        let available = (w.libraries.get(&w.region))
            .map(|lib| convert::slot_kinds(lib))
            .unwrap_or_default();
        let mut slot_table = div().flex().flex_col().w_full().gap_1().p_1();
        for (i, r) in w.slots.iter().enumerate() {
            let mut chips = div().flex().flex_wrap().gap_1().flex_1();
            for target in available.iter().copied().map(Some).chain([None]) {
                let name = target.map_or_else(|| "Leave out".to_owned(), convert::slot_label);
                chips = chips.child(
                    button(name, r.target == target)
                        .id(ElementId::Name(
                            format!("import-slot-{i}-{target:?}").into(),
                        ))
                        .text_size(px(11.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(r) = this.import.as_mut().and_then(|w| w.slots.get_mut(i)) {
                                r.target = target;
                            }
                            cx.notify();
                        })),
                );
            }
            slot_table = slot_table.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .px_1()
                    .py_1()
                    .text_size(px(12.))
                    .child(div().w(px(150.)).child(convert::slot_label(r.kind)))
                    .child(
                        div()
                            .w(px(40.))
                            .text_color(theme::get().muted)
                            .child(format!("×{}", r.count)),
                    )
                    .child(div().text_color(theme::get().muted).child("→"))
                    .child(chips),
            );
        }
        if w.slots.is_empty() {
            slot_table = slot_table.child(hint(if w.loading_library.is_some() {
                "Reading this province’s slot records…".to_owned()
            } else if w.source.is_some() {
                "The island has no resource slots".into()
            } else {
                "Pick an island".into()
            }));
        }
        // The Ground tab: each source material, its share, what it becomes.
        let from_short = w.from.profile().short_name;
        let set = w.target().and_then(|s| w.sets.get(&s.path)).cloned();
        let source_material = |id: u8| {
            (w.source.as_ref())
                .and_then(|s| s.thumbs.iter().find(|t| t.0 == id))
                .map(|t| t.1.clone())
        };
        let target_material = |id: u8| {
            let t = set.as_ref()?;
            let thumb = t.thumbs.iter().find(|x| x.0 == id).map(|x| x.1.clone());
            let name = t
                .palette
                .materials
                .get(&id)
                .map(|m| stem(&m.file).to_owned())?;
            Some((thumb, name))
        };
        let brought = (w.ground.iter())
            .filter(|r| r.pick == convert::GroundPick::Bring)
            .count();
        // Material ids still free for brought textures. With the island's own
        // set that's 32 minus what it already holds (brought textures plus
        // hand-picked target ones); otherwise the target set's free ids.
        let own = w.own_ground;
        let picked_targets = {
            let mut t: Vec<u8> = (w.ground.iter())
                .filter_map(|r| match r.pick {
                    convert::GroundPick::Material(id) => Some(id),
                    convert::GroundPick::Bring => None,
                })
                .collect();
            t.sort_unstable();
            t.dedup();
            t.len()
        };
        let free = match own {
            true => 32usize.saturating_sub(brought + picked_targets),
            false => set.as_ref().map_or(0, |t| t.free()).saturating_sub(brought),
        };
        let mut ground_table = div().flex().flex_col().w_full();
        for (i, r) in w.ground.iter().enumerate() {
            let first = r.ids.first().copied().unwrap_or(0);
            let (image, name) = match r.pick {
                convert::GroundPick::Bring => (
                    source_material(first),
                    format!("→ Anno {from_short} texture, brought"),
                ),
                convert::GroundPick::Material(id) => match target_material(id) {
                    Some((image, name)) => (image, format!("→ {name}")),
                    None => (None, format!("→ material {id}")),
                },
            };
            ground_table = ground_table.child(
                div()
                    .id(ElementId::Name(format!("import-ground-{i}").into()))
                    .flex()
                    .gap_2()
                    .px_2()
                    .py(px(2.))
                    .items_center()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme::get().hover))
                    .tooltip(Tooltip::text(
                        r.file.clone(),
                        "",
                        "Click to pick what it becomes",
                    ))
                    .child(thumb(source_material(first), 28.))
                    .child(div().w(px(200.)).truncate().child(stem(&r.file).to_owned()))
                    .child(
                        div()
                            .w(px(52.))
                            .text_color(theme::get().muted)
                            .child(format!("{:.1}%", r.share * 100.)),
                    )
                    .child(
                        div()
                            .w(px(70.))
                            .text_color(theme::get().muted)
                            .child(r.kind.label()),
                    )
                    .child(thumb(image, 28.))
                    .child(div().flex_1().truncate().child(name))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.picking_ground = Some(i);
                        }
                        cx.notify();
                    })),
            );
        }
        if w.ground.is_empty() {
            ground_table =
                ground_table.child(div().p_1().child(hint(if w.loading_set.is_some() {
                    "Reading this province’s ground materials…".to_owned()
                } else if w.source.is_some() {
                    "The island shows no ground materials".into()
                } else {
                    "Pick an island".into()
                })));
        }
        let ground_picker = w.picking_ground.and_then(|i| w.ground.get(i)).map(|row| {
            let first = row.ids.first().copied().unwrap_or(0);
            let tile = |id: String,
                        image: Option<Arc<RenderImage>>,
                        name: String,
                        current: bool,
                        enabled: bool,
                        pick: convert::GroundPick| {
                picker_tile(id, thumb(image, 72.), name.clone(), current, enabled)
                    .tooltip(Tooltip::text(name, "", ""))
                    .when(enabled, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            this.import_choose_ground(pick);
                            cx.notify();
                        }))
                    })
            };
            let mut grid = div().flex().flex_wrap().gap_1().p_1().w_full();
            let is_bring = row.pick == convert::GroundPick::Bring;
            // Free ids left once the other brought rows take theirs.
            let left = free + usize::from(is_bring);
            if !same {
                grid = grid.child(tile(
                    "import-ground-bring".into(),
                    source_material(first),
                    format!("Anno {from_short} texture"),
                    is_bring,
                    left > 0,
                    convert::GroundPick::Bring,
                ));
            }
            if let Some(t) = &set {
                for (&id, m) in &t.palette.materials {
                    let image = t.thumbs.iter().find(|x| x.0 == id).map(|x| x.1.clone());
                    let mut name = stem(&m.file).to_owned();
                    if id == row.auto {
                        name = format!("{name} (closest)");
                    }
                    grid = grid.child(tile(
                        format!("import-ground-{id}"),
                        image,
                        name,
                        row.pick == convert::GroundPick::Material(id),
                        true,
                        convert::GroundPick::Material(id),
                    ));
                }
            }
            let header = div()
                .flex()
                .items_center()
                .gap_2()
                .child(thumb(source_material(first), 56.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.))
                                .truncate()
                                .child(format!("Replace {}", stem(&row.file))),
                        )
                        .child(hint(format!(
                            "{:.1}% of the ground · {} · the set has {} for brought textures",
                            row.share * 100.,
                            row.kind.label(),
                            crate::project::plural(left, "free slot", "free slots")
                        ))),
                )
                .child(
                    button("Back", false).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.picking_ground = None;
                        }
                        cx.notify();
                    })),
                );
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(header)
                .child(hint(format!(
                    "The Anno {from_short} texture is copied into the mod and goes into this island’s own material set"
                )))
                .child(
                    vscroll("import-ground-picker", &self.scrolls.get("import-ground-picker"), grid)
                        .flex_1()
                        .min_h_0()
                        .bg(theme::get().input),
                )
                .into_any_element()
        });
        let ground_view = match ground_picker {
            Some(picker) => picker,
            None => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap_1()
                // Converting regions maps onto the province's own ground;
                // keeping the source's would copy this game's textures into
                // the mod for nothing.
                .when(!same, |d| d.child(
                    checkbox(
                        format!("Keep the Anno {from_short} ground (the island gets its own material set)"),
                        own,
                        "",
                    )
                    .id("import-own-ground")
                    .tooltip(Tooltip::text(
                        "Own material set",
                        "",
                        format!(
                            "Copies every ground texture into the mod, into a material set only this island uses, so the ground looks as it did. Turned off, each texture is matched to one of the province’s {} textures instead",
                            self.module.profile().short_name
                        ),
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.own_ground = !w.own_ground;
                            w.ground_chosen.clear();
                        }
                        this.import_remap();
                        cx.notify();
                    })),
                ))
                .child(
                    vscroll("import-ground", &self.scrolls.get("import-ground"), ground_table)
                        .flex_1()
                        .min_h_0()
                        .bg(theme::get().input),
                )
                .into_any_element(),
        };
        let mapping = match w.picking.and_then(|i| w.rows.get(i)) {
            _ if w.tab == Tab::Ground => ground_view,
            None if w.tab == Tab::Slots => vscroll(
                "import-slots",
                &self.scrolls.get("import-slots"),
                slot_table,
            )
            .flex_1()
            .min_h_0()
            .bg(theme::get().input)
            .into_any_element(),
            None => vscroll("import-props", &self.scrolls.get("import-props"), table)
                .flex_1()
                .min_h_0()
                .bg(theme::get().input)
                .into_any_element(),
            Some(row) => {
                let tile = |id: String,
                            image: Option<Arc<RenderImage>>,
                            name: String,
                            current: bool,
                            choose: Option<String>| {
                    let picture = match &choose {
                        Some(_) => thumb(image, 72.),
                        None => square(72.).text_size(px(24.)).child("✕"),
                    };
                    picker_tile(id, picture, name, current, true)
                        .tooltip(Tooltip::text(
                            choose.clone().unwrap_or_else(|| "Leave out".into()),
                            "",
                            "",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.import_choose(choose.clone());
                            cx.notify();
                        }))
                };
                let mut grid = div().flex().flex_wrap().gap_1().p_1().w_full();
                grid = grid.child(tile(
                    "import-tile-none".into(),
                    None,
                    "Leave out".into(),
                    row.pick.is_none(),
                    None,
                ));
                if same {
                    grid = grid.child(tile(
                        "import-tile-bring".into(),
                        source_thumb(&row.source),
                        "Keep as is".into(),
                        row.target() == Some(row.source.as_str()),
                        Some(row.source.clone()),
                    ));
                } else if port::portable(w.from, self.module, &row.source) {
                    let ported = port::target(w.from, self.module, &row.source);
                    grid = grid.child(tile(
                        "import-tile-bring".into(),
                        source_thumb(&row.source),
                        format!("Anno {} model", w.from.profile().short_name),
                        row.target() == Some(ported.as_str()),
                        Some(ported),
                    ));
                }
                for (k, cfg) in offers.iter().enumerate() {
                    grid = grid.child(tile(
                        format!("import-tile-{k}"),
                        target_thumb(cfg),
                        stem(cfg).to_owned(),
                        row.target() == Some(cfg.as_str()),
                        Some(cfg.clone()),
                    ));
                }
                let header = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(thumb(source_thumb(&row.source), 56.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .truncate()
                                    .child(format!("Replace {}", stem(&row.source))),
                            )
                            .child(hint(format!(
                                "×{} · {} · {} offered",
                                row.count,
                                row.role.map_or("unknown kind", |r| r.label()),
                                offers.len()
                            ))),
                    )
                    .child(
                        button("Back", false).on_click(cx.listener(|this, _, _, cx| {
                            if let Some(w) = &mut this.import {
                                w.picking = None;
                                w.typing = Typing::Filter;
                            }
                            cx.notify();
                        })),
                    );
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(header)
                    .child(field(&w.search, w.typing == Typing::Search))
                    .child(hint(if w.search.text.trim().is_empty() {
                        "Suggested first, then every model of this kind. Type to search all props"
                            .to_owned()
                    } else {
                        "Every prop whose path has all the words".into()
                    }))
                    .child(
                        vscroll("import-picker", &self.scrolls.get("import-picker"), grid)
                            .flex_1()
                            .min_h_0()
                            .bg(theme::get().input),
                    )
                    .into_any_element()
            }
        };
        let right = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(hint(format!("Into province ({})", self.module.profile().title)))
            .child(regions)
            .child(hint(format!("Size: {size}")))
            .child(hint("Name".to_owned()))
            .child(
                div()
                    .id("import-name")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.typing = Typing::Name;
                        }
                        cx.notify();
                    }))
                    .child(field(&w.name, w.typing == Typing::Name)),
            )
            .child(options)
            .child({
                // How poor a look match has to be before the source game's own
                // model comes instead. Dragging only moves the value and the
                // confidence highlight. The rows rematch once, on release,
                // because rematching on every step made the drag stutter. Hand
                // picks are kept.
                let from = w.from.profile().short_name;
                let bring = field::slider_field(
                    "import-bring-below",
                    "Import below",
                    format!("{:.0}%", w.bring_below * 100.),
                    w.bring_below,
                    false,
                )
                .tooltip(Tooltip::text(
                    "Import below",
                    "",
                    if same {
                        "Props whose best match in the province is less alike than this keep their own model. 0% never, 100% always".to_owned()
                    } else {
                        format!(
                            "Props whose best match is less alike than this come over as the Anno {from} model, copied into the mod. 0% never, 100% always (when the model can be ported)"
                        )
                    },
                ))
                .on_drag_move(cx.listener(
                    |this, e: &DragMoveEvent<field::SliderDrag>, _, cx| {
                        if e.drag(cx).0.as_ref() != "import-bring-below" {
                            return;
                        }
                        let f = (field::slider_fraction(e) * 100.).round() / 100.;
                        let Some(w) = &mut this.import else { return };
                        if w.bring_below == f {
                            return;
                        }
                        w.bring_below = f;
                        cx.notify();
                    },
                ))
                // The drag ends wherever the button comes up: over the
                // slider or anywhere else.
                .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.import_rematch_if_moved();
                    cx.notify();
                }))
                .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.import_rematch_if_moved();
                    cx.notify();
                }));
                let tab = |name: String, which: Tab| {
                    button(name, w.tab == which)
                        .text_size(px(12.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(w) = &mut this.import {
                                w.tab = which;
                                w.picking = None;
                                w.picking_ground = None;
                                w.typing = Typing::Filter;
                            }
                            cx.notify();
                        }))
                };
                let slots: usize = w.slots.iter().map(|r| r.count).sum();
                let kept: usize = (w.slots.iter())
                    .filter(|r| r.target.is_some())
                    .map(|r| r.count)
                    .sum();
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(tab(format!("Props ({})", w.rows.len()), Tab::Props))
                    .child(tab(format!("Ground ({})", w.ground.len()), Tab::Ground))
                    .child(tab(format!("Slots ({slots})"), Tab::Slots))
                    // The hint shrinks and truncates; the slider keeps its
                    // width at the end of the row. It used to shrink to nothing
                    // next to a long hint.
                    .child(div().pl_2().flex_1().min_w_0().truncate().child(hint(match w.tab {
                        _ if w.rows.is_empty() => "Pick an island".to_owned(),
                        Tab::Props if w.matched => format!(
                            "{mapped} of {placed} placed props mapped by look; click a row to pick its replacement"
                        ),
                        Tab::Props => format!(
                            "{mapped} of {placed} placed props mapped by name; measuring props to match by look…"
                        ),
                        Tab::Ground if w.ground.is_empty() => {
                            "Reading this province’s ground materials…".to_owned()
                        }
                        Tab::Ground => format!(
                            "{} ground textures, {brought} brought, {free} slots free; click one to pick what it becomes",
                            w.ground.len()
                        ),
                        Tab::Slots => format!(
                            "{kept} of {slots} slots kept; pick what each kind becomes here"
                        ),
                    }).truncate()))
                    .when(w.tab == Tab::Props, |d| d.child(bring.flex_none()))
            })
            .child(mapping);

        let ready = w.busy.is_none() && w.source.is_some() && !w.name.text.trim().is_empty();
        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child(w.busy.clone().unwrap_or_default()),
            )
            .child(
                button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                    this.import = None;
                    cx.notify();
                })),
            )
            .child(
                button(
                    if w.busy.as_deref() == Some("Converting…") {
                        "Converting…"
                    } else if same {
                        "Convert"
                    } else {
                        "Import"
                    },
                    ready,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.import_run(cx);
                    cx.notify();
                })),
            );

        let sources = self.import_sources();
        let source_picker = (!same && sources.len() > 1).then(|| {
            gpui_editor_kit::widgets::segmented(
                "import-from",
                (sources.iter()).map(|m| (m.game(), m.profile().title.into())),
                &w.from.game(),
                cx,
                |this, game, _, cx| {
                    if this.import.as_ref().is_some_and(|w| w.from.game() != game) {
                        this.open_import_from(crate::games::module(game), cx);
                    }
                },
            )
        });
        let card = gpui_editor_kit::widgets::dialog(if same {
            "Convert an island to another region".to_owned()
        } else {
            format!("Import an island from {}", w.from.profile().title)
        })
        .w(px(980.))
        .h(px(660.))
        // Which game the island comes from, where Import reads several.
        .when_some(source_picker, |d, picker| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(hint("From"))
                    .child(picker),
            )
        })
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .gap_4()
                .child(left)
                .child(right),
        )
        .child(footer);
        Some(gpui_editor_kit::widgets::modal("import", card))
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::Editor;
    use anno_core::game::Game;
    use gpui::{TestApp, TestAppWindow};

    /// The editor on `game`'s install (its `*_DIR`), with a fresh config
    /// folder kept while the test runs.
    fn editor(game: Game) -> Option<(TestApp, TestAppWindow<Editor>, tempfile::TempDir)> {
        let module = crate::games::module(game);
        let dir = std::env::var_os(module.profile().env_var)?;
        let mut app = TestApp::new();
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().to_path_buf();
        let w = app.open_window(|window, cx| {
            Editor::new(
                crate::project::Project::load(&folder, module).unwrap(),
                folder,
                Some(dir.into()),
                None,
                window,
                cx,
            )
        });
        app.run_until_parked();
        Some((app, w, config))
    }

    /// Imports `path` from `from` as the dialog does: open it on that game,
    /// pick the island, name it, convert, accept every loss the review asks
    /// about, then save. Gives the new island's path.
    fn import(app: &mut TestApp, w: &mut TestAppWindow<Editor>, from: Game, path: &str) -> String {
        w.update(|ed, _, cx| ed.open_import_from(crate::games::module(from), cx));
        app.run_until_parked();
        w.update(|ed, _, cx| {
            let wizard = ed.import.as_ref().expect("the dialog is open");
            assert!(wizard.busy.is_none(), "{:?}", wizard.busy);
            assert!(
                wizard.islands.iter().any(|s| s.path == path),
                "{} islands listed, not {path}",
                wizard.islands.len()
            );
            ed.import_pick(path.to_owned(), cx);
        });
        app.run_until_parked();
        w.update(|ed, _, cx| {
            let wizard = ed.import.as_mut().unwrap();
            assert!(wizard.source.is_some(), "the island was read");
            wizard.name.text = "Brought Over".into();
            ed.import_run(cx);
        });
        app.run_until_parked();
        w.update(|ed, _, _| {
            if ed.import.as_ref().is_some_and(|w| w.review.is_some()) {
                ed.import_accept_all();
            }
            assert!(ed.import.is_none(), "the dialog stayed open");
            ed.save();
            let island = ed.island.as_ref().expect("the import opened");
            assert!(!island.dirty(), "the save failed");
            island.path.clone()
        })
    }

    // Needs Anno 117 and 2070: a 2070 project offers Import from 117 and
    // 1800, and brings a small 117 island over through the dialog: onto a
    // copy of a surface template, with 117's trees and rocks as 2070
    // models from its own islands, saved through 2070's codec into the
    // project and opened again from there.
    #[test]
    fn a_2070_project_imports_a_117_island_through_the_dialog() {
        if anno_formats::vfs::install_dir("ANNO117_DIR").is_none() {
            return;
        }
        let Some((mut app, mut w, _config)) = editor(Game::Anno2070) else {
            return;
        };
        w.read(|ed, _| {
            let sources: Vec<Game> = ed.import_sources().iter().map(|m| m.game()).collect();
            assert_eq!(sources, [Game::Anno117, Game::Anno1800]);
            assert_eq!(ed.import_label(), "Import island…");
        });
        let path = import(
            &mut app,
            &mut w,
            Game::Anno117,
            crate::island::test_support::SMALL,
        );
        assert_eq!(path, "data/levels/islands/normal/brought_over.isd");
        w.update(|ed, _, _| {
            let entry = (ed.project.islands.iter()).find(|i| i.path == path);
            assert!(entry.is_some(), "the island joined the project");
            let island = ed.island.as_ref().unwrap();
            assert!(
                !island.island.doc.props.is_empty(),
                "117's vegetation came over"
            );
            let saved = ed.project.read_file(&path).expect("saved into the project");
            let doc = anno_formats::isd::Doc::parse(&saved).unwrap();
            let records = anno_formats::isd::objects::read(&doc, &path).unwrap();
            assert!(records.len() >= island.island.doc.props.len());
            let install = ed.install.clone().unwrap();
            let back = crate::island::IslandEditor::load(ed.module, &install, &path).unwrap();
            assert_eq!(
                back.island.doc.field.heights,
                island.island.doc.field.heights
            );
        });
    }

    // Needs Anno 117 and 2070: a 117 project imports 2070's n_m02 through
    // the dialog's other source: a new 117 island of 2070's ground, saved
    // into the project.
    #[test]
    fn a_117_project_imports_a_2070_island_through_the_dialog() {
        if anno_formats::vfs::install_dir("ANNO2070_DIR").is_none() {
            return;
        }
        let Some((mut app, mut w, _config)) = editor(Game::Anno117) else {
            return;
        };
        w.read(|ed, _| {
            let sources: Vec<Game> = ed.import_sources().iter().map(|m| m.game()).collect();
            assert_eq!(sources, [Game::Anno1800, Game::Anno2070]);
        });
        let path = import(
            &mut app,
            &mut w,
            Game::Anno2070,
            "data/levels/islands/normal/n_m02.isd",
        );
        // 2070's surface islands start in Albion.
        assert!(path.ends_with("/brought_over/brought_over.a7m"), "{path}");
        assert!(path.contains("/celtic/"), "{path}");
        w.read(|ed, _| {
            assert!(ed.project.islands.iter().any(|i| i.path == path));
            let island = ed.island.as_ref().unwrap();
            let land = island
                .island
                .doc
                .field
                .heights
                .iter()
                .filter(|&&h| h > 1.)
                .count();
            assert!(land > 1000, "n_m02's land came over: {land} corners");
        });
    }

    // Import suggests the province closest in climate, by the target game's
    // own table: 1800's Arctic lands in Albion, 117's islands in the Old
    // World, and a region the table lacks suggests none.
    #[test]
    fn import_suggests_the_targets_closest_province() {
        let suggest = |game, from, regions: &[&str]| {
            let regions: Vec<String> = regions.iter().map(|r| r.to_string()).collect();
            super::suggest_region(crate::games::rules(game), from, &regions)
        };
        let provinces = ["roman", "celtic", "egypt"];
        assert_eq!(
            suggest(Game::Anno117, "arctic", &provinces).as_deref(),
            Some("celtic")
        );
        assert_eq!(
            suggest(Game::Anno117, "africa", &provinces).as_deref(),
            Some("egypt")
        );
        assert_eq!(suggest(Game::Anno117, "roman", &provinces), None);
        let old_world = ["moderate", "colony01"];
        assert_eq!(
            suggest(Game::Anno1800, "celtic", &old_world).as_deref(),
            Some("moderate")
        );
    }
}
