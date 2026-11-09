// The Import wizard: picks an island of the other installed game, maps its
// props and ground onto a province of this one, and builds the result as a
// new island. The conversion itself is `convert`.
use super::{
    IslandEditor, Skeleton, Spec, Start,
    catalog::{self, Catalog, Thumbs},
    convert::{self, Options, PropRow, SlotRow},
    harbour, islands, port, skeletons,
    wizard::province,
};
use crate::editor::{Editor, Workspace};
use anno_formats::{assets::Assets, game::Game, vfs::Install};
use anyhow::Context as _;
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit},
    gpui::{prelude::*, *},
    theme,
    widgets::{Tooltip, button, checkbox, hint, vscroll},
};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Import {
    from: Game,
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
    options: Options,
    /// What is running in the background, if anything.
    busy: Option<String>,
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
    Slots,
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
    fn offers(&self, catalog: &Catalog, game: Game, row: &PropRow) -> Vec<String> {
        let words: Vec<String> = (self.search.text.split_whitespace())
            .map(str::to_ascii_lowercase)
            .collect();
        let mut out: Vec<String> = (row.choices.iter())
            .filter(|c| !c.starts_with(port::FOLDER))
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
                let fit = convert::fit(game, &self.region, &e.path).unwrap_or(2);
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

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".a7m")
        .or_else(|| name.rsplit_once('.').map(|(s, _)| s))
        .unwrap_or(name)
}

/// The province of this game closest in climate to `from` of the other:
/// Albion's temperate north for the Old World and Arctic, Latium for the
/// warm ones; 117 islands land in the Old World.
fn suggest_region(from: &str, regions: &[String]) -> Option<String> {
    let want = match from {
        "moderate" | "arctic" => "celtic",
        "colony01" | "africa" => "roman",
        _ => "moderate",
    };
    regions.iter().find(|r| r.as_str() == want).cloned()
}

impl Editor {
    /// File → Import from Anno 1800 / 117…
    pub fn open_import(&mut self, cx: &mut Context<Self>) {
        let (Some(_), Some(assets)) = (&self.install, &self.assets) else {
            return self.message("The game is still loading; try again in a moment", true);
        };
        if self.island.as_ref().is_some_and(|i| i.dirty()) {
            return self.message("Save the open island (Ctrl S) before importing one", true);
        }
        let from = self.game.profile().import_from;
        let Some(dir) = crate::launcher::install_dir(from) else {
            return self.message(
                format!(
                    "{} is not installed here, or its folder is unknown: open it once from the game picker",
                    from.profile().title
                ),
                true,
            );
        };
        let skeletons = skeletons(self.install.as_ref().expect("checked"), Some(assets));
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
            options: Options::default(),
            busy: Some(format!("Opening {}…", from.profile().title)),
        });
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    let install = Arc::new(Install::open_game(from, &dir)?);
                    let assets = Arc::new(Assets::load(&install)?);
                    let list = islands(&install, Some(&assets));
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
            if w.picking.take().is_some() {
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
        let (Some(target), cache, catalog) = (
            self.install.clone(),
            self.config.join("catalog.json"),
            self.catalog.clone(),
        ) else {
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
        if let Some(from) = w.islands.iter().find(|s| s.path == path)
            && let Some(region) = suggest_region(&from.region, &w.regions())
        {
            w.region = region;
        }
        w.source = None;
        w.rows.clear();
        w.chosen.clear();
        w.name.text = format!("{} {}", stem(&path), w.from.profile().short_name);
        w.busy = Some(format!("Reading {}…", stem(&path)));
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let mut src = IslandEditor::load(&install, &path)?;
                    src.assets = Some(assets);
                    let catalog = match catalog {
                        Some(c) => c,
                        None => Arc::new(Catalog::load_or_build(&target, &cache)?),
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
                            w.source_thumbs = Some(Thumbs::start(install.clone(), dir));
                        }
                        this.catalog.get_or_insert(catalog);
                        this.import_remap();
                        this.import_library(cx);
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

    /// A fresh prop mapping for the picked island and province.
    pub(super) fn import_remap(&mut self) {
        // Ask for every look the match needs; cached ones arrive within a
        // poll, so `pending` below is true until all are in.
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            self.thumbs = Some(catalog::Thumbs::start(
                install,
                self.config.join("thumbnails"),
            ));
        }
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
        w.rows = convert::map_props(
            &convert::source_models(src),
            catalog,
            (w.from, self.game),
            &w.region,
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
            .map(|s| install.game().island_region(&s.path).to_owned())
        else {
            return;
        };
        let region = w.region.clone();
        w.loading_library = Some(region.clone());
        cx.spawn(async move |this, cx| {
            let lib = cx
                .background_executor()
                .spawn(async move { harbour::Library::load(&install, &assets, &game_region) })
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
        let spec = Spec {
            name: w.name.text.clone(),
            skeleton,
            start: Start::Sea,
            listing: crate::project::Listing::default(),
        };
        let (mut rows, slots, options) = (w.rows.clone(), w.slots.clone(), w.options);
        // Should a port fail, the row takes its best match of this game.
        let look = |t: &Option<Thumbs>, cfg: &str| t.as_ref().and_then(|t| t.look(cfg));
        let fallback: Vec<Option<(usize, f32)>> = (rows.iter())
            .map(|r| {
                let k = r
                    .choices
                    .iter()
                    .position(|c| !c.starts_with(port::FOLDER))?;
                let scale = convert::height_scale(
                    look(&w.source_thumbs, &r.source),
                    look(&self.thumbs, &r.choices[k]),
                );
                Some((k, scale))
            })
            .collect();
        let other = w.other.as_ref().map(|o| o.0.clone());
        let lib = w.libraries.get(&w.region).cloned();
        w.busy = Some("Converting…".into());
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let mut dst = IslandEditor::create(&install, &spec)?;
                    dst.harbour = match lib {
                        Some(lib) => Some(lib),
                        None => harbour::Library::load(&install, &assets, &dst.region())
                            .map_err(|e| eprintln!("import: harbour records: {e:#}"))
                            .ok()
                            .map(Arc::new),
                    };
                    dst.assets = Some(assets);
                    let mut ported = Vec::new();
                    for (r, fallback) in rows.iter_mut().zip(fallback) {
                        if !r.brings() {
                            continue;
                        }
                        let done = (other.as_deref())
                            .context("the other game is not open")
                            .and_then(|from| port::port(from, &install, &r.source));
                        match done {
                            Ok(p) => ported.extend(p.files),
                            Err(e) => {
                                eprintln!("import: {}: {e:#}", r.source);
                                (r.pick, r.scale) = match fallback {
                                    Some((k, scale)) => (Some(k), scale),
                                    None => (None, 1.),
                                };
                            }
                        }
                    }
                    let report = convert::convert(&src, &mut dst, &rows, &slots, &options);
                    anyhow::Ok((dst, report, src, ported))
                })
                .await;
            this.update(cx, |this, cx| {
                match built {
                    Ok((mut island, report, _, ported)) => {
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
                        island.say(format!("Imported: {}", report.summary()), false);
                        this.import = None;
                        this.island = Some(island);
                        this.switch(Workspace::Island);
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

    pub fn render_import(&mut self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        // Thumbnails first: the table's two sides, and the picker's tiles.
        let w = self.import.as_ref()?;
        let offers = match (w.picking.and_then(|i| w.rows.get(i)), &self.catalog) {
            (Some(row), Some(catalog)) => w.offers(catalog, self.game, row),
            _ => Vec::new(),
        };
        let sources: Vec<String> = w.rows.iter().map(|r| r.source.clone()).collect();
        let mut targets: Vec<String> = (w.rows.iter())
            .filter_map(|r| r.target().map(str::to_owned))
            .collect();
        targets.extend(offers.iter().cloned());
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            let dir = self.config.join("thumbnails");
            self.thumbs = Some(catalog::Thumbs::start(install, dir));
        }
        if let Some(t) = &mut self.thumbs {
            t.request(targets.iter().map(String::as_str));
        }
        if let Some(t) = self.import.as_mut().and_then(|w| w.source_thumbs.as_mut()) {
            t.request(sources.iter().map(String::as_str));
        }
        self.poll_thumbs(cx);
        let w = self.import.as_ref()?;
        let thumb = |image: Option<Arc<RenderImage>>, size: f32| {
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
                .child(match image {
                    Some(image) => img(image).size(px(size)).into_any_element(),
                    None => "…".into_any_element(),
                })
        };
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
                        province(&s.region),
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
            .child(hint(format!(
                "Island from {} ({} shipped; type to filter)",
                w.from.profile().title,
                w.islands.len()
            )))
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
                button(province(&region), w.region == region)
                    .text_size(px(12.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(w) = &mut this.import {
                            w.region = r.clone();
                        }
                        this.import_remap();
                        this.import_library(cx);
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
                                Some(c) if c < convert::BRING_BELOW => theme::get().highlight,
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
        let mapping = match w.picking.and_then(|i| w.rows.get(i)) {
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
                    div()
                        .id(ElementId::Name(id.into()))
                        .w(px(84.))
                        .p_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(current, |d| d.bg(theme::get().selected))
                        .hover(|d| d.bg(theme::get().hover))
                        .tooltip(Tooltip::text(
                            choose.clone().unwrap_or_else(|| "Leave out".into()),
                            "",
                            "",
                        ))
                        .child(match &choose {
                            Some(_) => thumb(image, 72.),
                            None => div()
                                .size(px(72.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_sm()
                                .bg(theme::get().bg)
                                .text_size(px(24.))
                                .text_color(theme::get().muted)
                                .child("✕"),
                        })
                        .child(div().w_full().text_size(px(10.)).truncate().child(name))
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
                if port::portable(w.from, self.game, &row.source) {
                    let ported = port::target(&row.source);
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
            .child(hint(format!("Into province ({})", self.game.profile().title)))
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
                let tab = |name: String, which: Tab| {
                    button(name, w.tab == which)
                        .text_size(px(12.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(w) = &mut this.import {
                                w.tab = which;
                                w.picking = None;
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
                    .child(tab(format!("Slots ({slots})"), Tab::Slots))
                    .child(div().pl_2().child(hint(match w.tab {
                        _ if w.rows.is_empty() => "Pick an island".to_owned(),
                        Tab::Props if w.matched => format!(
                            "{mapped} of {placed} placed props mapped by look; click a row to pick its replacement"
                        ),
                        Tab::Props => format!(
                            "{mapped} of {placed} placed props mapped by name; measuring props to match by look…"
                        ),
                        Tab::Slots => format!(
                            "{kept} of {slots} slots kept; pick what each kind becomes here"
                        ),
                    })))
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

        let card = gpui_editor_kit::widgets::dialog(format!(
            "Import an island from {}",
            w.from.profile().title
        ))
        .w(px(980.))
        .h(px(660.))
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
