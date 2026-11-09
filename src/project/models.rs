// The Project workspace's Models pane: every model the project carries
// (ported from the other game, or added from disk) and every model its
// dependencies provide, with which project islands use them. Adding,
// removing and placing them; the Library's "This mod" tab lists the same.
use super::Project;
use crate::editor::{Editor, Workspace};
use crate::island::{catalog, port};
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    theme,
    widgets::{self, Tone, Tooltip, action, badge, confirm_button, empty_state, hint, segmented},
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A Models card: its entry, its picture once rendered, and whether its
/// file is missing.
type Card = (Entry, Option<Arc<RenderImage>>, bool);

/// Which models the pane shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Project,
    Dependencies,
}

/// Where a listed model comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Ported,
    Custom,
    /// A dependency's name.
    Mod(String),
}

impl Source {
    pub fn label(&self) -> &str {
        match self {
            Self::Ported => "ported",
            Self::Custom => "custom",
            Self::Mod(name) => name,
        }
    }

    fn color(&self) -> Rgba {
        match self {
            Self::Ported => crate::colors::get().ported,
            Self::Custom => theme::get().accent,
            Self::Mod(_) => theme::get().ok,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub cfg: String,
    pub source: Source,
}

/// Model files (cfg, prp) staged in the project.
pub fn project_cfg_files(project: &Project) -> Vec<String> {
    port::project_models(&project.files)
        .into_iter()
        .flat_map(|m| m.files)
        .collect()
}

/// The model's name: its file name without extension.
pub fn model_name(cfg: &str) -> &str {
    let name = cfg.rsplit('/').next().unwrap_or(cfg);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

impl Editor {
    /// The two ways to get a model, side by side with what each needs said
    /// under it, and where models from other mods come from. Shared by the
    /// Models page and the Library's This mod tab, so the one empty state
    /// reads the same in both.
    pub(crate) fn model_ways(&self, in_library: bool, cx: &mut Context<Self>) -> Div {
        // Two equal framed tiles, side by side: as bare buttons over
        // captions of different widths they sat 440 px apart and read as
        // two unrelated buttons, not one choice. The whole tile answers the
        // pointer and the click: a frame that ignored them read as
        // decoration around a button.
        let choice = |id: &'static str,
                      button: Stateful<Div>,
                      caption: &str,
                      go: fn(&mut Editor, &mut Context<Editor>)| {
            div()
                .id(id)
                .w(px(320.))
                .p_3()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .rounded(px(4.))
                .border_1()
                .border_color(theme::get().border)
                .cursor_pointer()
                .hover(|s| s.border_color(theme::get().accent))
                .on_click(cx.listener(move |this, _, _, cx| go(this, cx)))
                .child(button)
                .child(hint(caption.to_owned()).text_center())
        };
        // "From disk": beside Browse game models, a bare "Add model" could
        // mean adding one of the game's.
        let add = action("Add model from disk…", Tone::Primary)
            .tooltip(Tooltip::text(
                "Add model",
                "",
                crate::game_text::of(self.game).add_model,
            ))
            // Its own click only: inside a tile that takes clicks too, the
            // picker would open twice.
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.add_model(cx)
            }));
        // The game's own models, where they can be previewed and placed:
        // Assets on its Models type. Routed through the Islands list (props
        // come from a region) it read as a detour for someone on a Models
        // page, and its label changed with whether an island was open.
        let browse = action("Browse game models →", Tone::Secondary).on_click(cx.listener(
            |this, _, _, cx| {
                cx.stop_propagation();
                this.browse_game_models(cx);
            },
        ));
        let where_to = "Preview any model in the game’s archives and place it";
        let link = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .text_size(px(11.))
                .text_color(theme::get().accent)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(label)
        };
        let to_dependencies = |id| {
            link(id, "Dependencies →").on_click(cx.listener(|this, _, _, cx| {
                this.show_pane(crate::editor::Pane::ProjectMods);
                cx.notify();
            }))
        };
        if in_library {
            // In a dock one card row deep: the button alone (the formats on
            // its tooltip), and where else models come from on one line.
            // Not the game's props: the Props tab sits beside This mod, and
            // the Place strip above already names it.
            return div()
                .flex()
                .flex_col()
                .items_center()
                .child(div().pt_2().child(add))
                .child(
                    div()
                        .pt_2()
                        .flex()
                        .gap(px(2.))
                        .child(hint("Another mod’s models:"))
                        .child(to_dependencies("library-dependencies")),
                );
        }
        div()
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .pt_2()
                    .flex()
                    .gap_3()
                    .child(choice(
                        "models-add-tile",
                        add,
                        crate::game_text::of(self.game).model_formats,
                        |this, cx| this.add_model(cx),
                    ))
                    .child(choice(
                        "models-browse-tile",
                        browse,
                        where_to,
                        |this, cx| this.browse_game_models(cx),
                    )),
            )
            // The tab's name as the link, in the sentence.
            .child(
                div()
                    .pt_2()
                    .flex()
                    .gap(px(2.))
                    .child(hint("Another mod’s models:"))
                    .child(to_dependencies("models-to-dependencies")),
            )
    }

    /// Project models, then each dependency's, in dependency order.
    pub fn model_entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = port::project_models(&self.project.files)
            .into_iter()
            .map(|m| Entry {
                cfg: m.cfg,
                source: match m.kind {
                    port::ModelKind::Ported => Source::Ported,
                    port::ModelKind::Custom => Source::Custom,
                },
            })
            .collect();
        let suffix = self.game.profile().prop_ext;
        if let Some(install) = &self.install {
            for (layer, id) in self.mounted.iter().enumerate() {
                let name = (self.mods.iter())
                    .find(|m| m.info.id == *id)
                    .map_or_else(|| id.clone(), |m| m.info.name.clone());
                out.extend(
                    install
                        .mod_files(layer, suffix)
                        .into_iter()
                        .map(|cfg| Entry {
                            cfg,
                            source: Source::Mod(name.clone()),
                        }),
                );
            }
        }
        out
    }

    /// Scans the project's islands for the models they place, off the UI
    /// thread, once per change to the project.
    fn ensure_usage(&mut self, cx: &mut Context<Self>) {
        if self.project_ui.usage.is_some() || self.project_ui.usage_loading {
            return;
        }
        let Some(install) = self.install.clone() else {
            return;
        };
        self.project_ui.usage_loading = true;
        let islands: Vec<String> = self
            .project
            .islands
            .iter()
            .map(|i| i.path.clone())
            .collect();
        cx.spawn(async move |this, cx| {
            let usage = cx
                .background_executor()
                .spawn(async move {
                    let mut usage: BTreeMap<String, Vec<String>> = BTreeMap::new();
                    for island in islands {
                        match catalog::island_models(&install, &island) {
                            Ok(models) => {
                                for m in models {
                                    usage.entry(m).or_default().push(island.clone());
                                }
                            }
                            Err(e) => eprintln!("models of {island}: {e:#}"),
                        }
                    }
                    usage
                })
                .await;
            this.update(cx, |this, cx| {
                this.project_ui.usage = Some(Arc::new(usage));
                this.project_ui.usage_loading = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Project islands placing `cfg`, as saved, plus the open island's
    /// unsaved state.
    fn used_by(&self, cfg: &str) -> Vec<String> {
        let mut out: Vec<String> = (self.project_ui.usage.as_ref())
            .and_then(|u| u.get(cfg).cloned())
            .unwrap_or_default();
        if let Some(island) = &self.island {
            let here = island.model_paths().iter().any(|m| m == cfg);
            out.retain(|p| *p != island.path || here);
            if here && !out.contains(&island.path) {
                out.push(island.path.clone());
            }
        }
        out
    }

    /// Models → Add model…
    pub fn add_model(&mut self, cx: &mut Context<Self>) {
        self.pick_path(
            "Add a model (.cfg, .prp, or a .rdm with its textures beside it)",
            false,
            cx,
            |this, path, _| this.import_model(&path),
        );
    }

    fn import_model(&mut self, path: &std::path::Path) {
        let Some(install) = self.install.clone() else {
            return self.message("No install is open", true);
        };
        let imported = match port::import(path, self.project.mod_id(), &install) {
            Ok(m) => m,
            Err(e) => return self.message(format!("Model not added: {e:#}"), true),
        };
        let replaced = self.project.files.contains_key(&imported.cfg);
        let count = imported.files.len();
        for (p, bytes) in imported.files {
            self.project.stage(&p, bytes);
        }
        if let Some(thumbs) = &mut self.thumbs {
            thumbs.invalidate(&imported.cfg);
        }
        self.project_ui.model = Some(imported.cfg.clone());
        self.project_ui.model_filter = Filter::Project;
        let name = model_name(&imported.cfg).to_owned();
        self.finish_project_edit(format!(
            "{} {name}: {count} file(s){}. Place it from Library → This mod",
            if replaced { "Replaced" } else { "Added" },
            match imported.shipped {
                0 => String::new(),
                n => format!(", {n} of the game’s files referenced"),
            }
        ));
    }

    fn remove_model(&mut self, cfg: &str) {
        let models = port::project_models(&self.project.files);
        let Some(model) = models.iter().find(|m| m.cfg == cfg) else {
            return;
        };
        let used = self.used_by(cfg);
        if !used.is_empty() {
            return self.message(
                format!(
                    "{} is placed on {}; take it off first",
                    model_name(cfg),
                    used.join(", ")
                ),
                true,
            );
        }
        let files = port::removable(&models, model);
        for f in &files {
            self.project.files.remove(f);
        }
        self.project_ui.model = None;
        self.finish_project_edit(format!(
            "Removed {} and {} file(s)",
            model_name(cfg),
            files.len()
        ));
    }

    /// Arms the open island's Place tool with `cfg`.
    fn place_model(&mut self, cfg: &str) {
        let Some(island) = &mut self.island else {
            return self.message("Open an island to place models on", true);
        };
        island.library_tab = catalog::Kind::Mod;
        island.choose_prop(cfg);
        self.switch(Workspace::Island);
        self.drain_messages();
    }

    pub fn render_project_models(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.install.is_none() {
            return empty_state("Indexing the install…").into_any_element();
        }
        self.ensure_usage(cx);
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            let dir = self.config.join("thumbnails");
            self.thumbs = Some(catalog::Thumbs::start(install, dir));
        }
        let all = self.model_entries();
        let filter = self.project_ui.model_filter;
        // As typed, as the Library's search filters: by shown name or path.
        let query = match &self.project_ui.editing {
            Some(e) if e.field == super::overview::Field::ModelSearch => e.text.clone(),
            _ => self.project_ui.model_query.clone(),
        }
        .trim()
        .to_lowercase();
        let (own, deps) = all
            .iter()
            .partition::<Vec<&Entry>, _>(|e| !matches!(e.source, Source::Mod(_)));
        // Without dependency models "All" is "This mod": shown as it.
        let filter = if deps.is_empty() && filter == Filter::All {
            Filter::Project
        } else {
            filter
        };
        let shown: Vec<Entry> = all
            .iter()
            .filter(|e| match filter {
                Filter::All => true,
                Filter::Project => !matches!(e.source, Source::Mod(_)),
                Filter::Dependencies => matches!(e.source, Source::Mod(_)),
            })
            // This mod's models no project island places, the ones safe to
            // remove: a check beside the search, as the Library's "Only on
            // this island", not a third source segment.
            .filter(|e| {
                !self.project_ui.models_unused_only
                    || !matches!(e.source, Source::Mod(_))
                        && self.project_ui.usage.is_some()
                        && self.used_by(&e.cfg).is_empty()
            })
            .filter(|e| {
                query.is_empty()
                    || e.cfg.to_lowercase().contains(&query)
                    || crate::island::pretty(model_name(&e.cfg))
                        .to_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect();
        // By the shown name, this mod's before its dependencies': in file
        // order "Factory, Wall lamp, Pebbles, Bush" read as no order at all.
        let mut shown = shown;
        shown.sort_by_cached_key(|e| {
            (
                matches!(e.source, Source::Mod(_)),
                crate::island::pretty(model_name(&e.cfg)).to_lowercase(),
            )
        });
        if let Some(thumbs) = &mut self.thumbs {
            thumbs.request(shown.iter().map(|e| e.cfg.as_str()));
        }
        self.poll_thumbs(cx);

        // Unused: removing an import meant opening each card to see its
        // islands. From dependencies only once a dependency brings any: "0"
        // sat as live as "This mod 25".
        let unused = own
            .iter()
            .filter(|e| self.project_ui.usage.is_some() && self.used_by(&e.cfg).is_empty())
            .count();
        let all_unused = unused == own.len() && !own.is_empty();
        // Filters on the left, actions on the right, as Assets and the
        // Library lay them out: a status sentence between the button and
        // the search read as the search's label.
        let toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .py_1()
            // The Library's search for the same models: finding one meant
            // scanning every tile.
            .child(self.detail_field(
                super::overview::Field::ModelSearch,
                &self.project_ui.model_query.clone(),
                "Search models…",
                cx,
            ))
            .child({
                // "All" only beside dependencies: without them it was a
                // second "This mod" with the same count.
                let mut options: Vec<(Filter, SharedString)> = Vec::new();
                if !deps.is_empty() {
                    options.push((Filter::All, format!("All {}", all.len()).into()));
                }
                options.push((Filter::Project, format!("This mod {}", own.len()).into()));
                if !deps.is_empty() || filter == Filter::Dependencies {
                    options.push((
                        Filter::Dependencies,
                        format!("From dependencies {}", deps.len()).into(),
                    ));
                }
                // One segment says nothing; the source shows on the tiles.
                let sources = (options.len() > 1).then(|| {
                    segmented("model-filter", options, &filter, cx, |this, f, _, _| {
                        this.project_ui.model_filter = f
                    })
                });
                let on = self.project_ui.models_unused_only;
                // All of them unused, the heading's line says so and a
                // filter would change nothing.
                div().flex().items_center().gap_2().children(sources).when(
                    (unused > 0 && !all_unused) || on,
                    |d| {
                        d.child(
                            widgets::checkbox(format!("Only unused · {unused}"), on, "").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.project_ui.models_unused_only =
                                        !this.project_ui.models_unused_only;
                                    cx.notify();
                                }),
                            ),
                        )
                    },
                )
            })
            .child(div().flex_1())
            // The game's own models stay a click away once the mod has
            // some: the empty state's link went with it.
            .child(
                div()
                    .id("models-browse-game")
                    .text_size(px(12.))
                    .text_color(theme::get().accent)
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .child("Browse game models →")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.browse_game_models(cx);
                        cx.notify();
                    })),
            )
            .child(
                // The page's one action, filled: beside a link it read
                // quieter than the link.
                action("Add model from disk…", Tone::Primary)
                    .tooltip(Tooltip::text(
                        "Add model",
                        "",
                        "A .cfg or .prp with its meshes and textures, or a bare .rdm (117) with its textures beside it",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.add_model(cx))),
            );

        let body: AnyElement = if shown.is_empty() && !query.is_empty() {
            empty_state("No model matches the search").into_any_element()
        } else if shown.is_empty() {
            empty_state(match filter {
                Filter::Dependencies => {
                    "No models from dependencies. Require a mod on the Dependencies tab to use its models."
                }
                _ => {
                    "No models in this mod yet. Add one, or import an island from the other game: its props come along."
                }
            })
            .child(action("Add model from disk…", Tone::Primary).on_click(cx.listener(|this, _, _, cx| this.add_model(cx))))
            .into_any_element()
        } else {
            let thumb = catalog::THUMB as f32;
            let selected = self.project_ui.model.clone();
            // The source badge only where sources differ: on a page of 25
            // ported models, 25 lavender pills outweighed the models.
            let mixed = shown.windows(2).any(|w| w[0].source != w[1].source);
            let cards: Arc<Vec<Card>> = Arc::new(
                shown
                    .into_iter()
                    .map(|e| {
                        let image = self.thumbs.as_ref().and_then(|t| t.get(&e.cfg));
                        let missing = self.thumbs.as_ref().is_some_and(|t| t.missing(&e.cfg));
                        (e, image, missing)
                    })
                    .collect(),
            );
            let entity = cx.entity().downgrade();
            let count = cards.len();
            let card_query = query.clone();
            widgets::card_grid(
                "project-models",
                count,
                thumb + 8.,
                window,
                cx,
                move |n, _, _| {
                    let (entry, image, missing) = &cards[n];
                    let cfg = entry.cfg.clone();
                    let entity = entity.clone();
                    widgets::thumbnail_card(
                        ("project-model", n),
                        image.clone(),
                        thumb,
                        // The Library's names: "Tree dead jungle 02", not the
                        // file name broken mid-word across two lines.
                        crate::island::pretty(model_name(&entry.cfg)),
                        &card_query,
                        if *missing { "No preview" } else { "…" },
                        selected.as_deref() == Some(entry.cfg.as_str()),
                    )
                    .when(mixed, |d| {
                        d.child(
                            badge(entry.source.label().to_owned(), entry.source.color())
                                .absolute()
                                .top(px(7.))
                                .left(px(7.)),
                        )
                    })
                    .tooltip(Tooltip::text(
                        model_name(&entry.cfg).to_owned(),
                        entry.source.label().to_owned(),
                        entry.cfg.clone(),
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.project_ui.model = Some(cfg.clone());
                            cx.notify();
                        });
                    })
                    .into_any_element()
                },
            )
        };

        // A mod with no models at all: one card in the Overview's column,
        // titled as the tab, with the two ways to get one.
        if all.is_empty() {
            // Unframed, a fixed step under the page's heading: centred in
            // the page it floated far from its title and read as a second
            // screen; in a card pinned top left it left the page blank.
            let body = div().w_full().pt(px(96.)).flex().justify_center().child(
                widgets::empty_state_icon(
                    gpui_editor_kit::icon::Icon::CUBE,
                    "No models in this mod yet",
                )
                .bg(transparent_black())
                .child(self.model_ways(false, cx)),
            );
            let page = super::overview::page(window).child(
                // The heading where the Overview's starts, so switching tabs
                // does not move it; the card spans the column with its
                // contents centred, as the Overview's banner spans it.
                super::overview::page_column(window)
                    .child(super::overview::page_header(
                        "Models",
                        "Models this mod ships, and those from the mods it depends on.",
                    ))
                    .child(body),
            );
            let handle = self.scrolls.get("project-models");
            return widgets::vscroll("project-models", &handle, page)
                .size_full()
                .bg(theme::get().panel)
                .into_any_element();
        }
        let detail = self.model_detail(&all, window, cx);
        // In the Overview's column under the same heading, as the empty
        // page is: full-bleed from the pane's edge, switching tabs moved the
        // content's left edge from 230 px to 10 and back.
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(
                super::overview::page(window).flex_1().min_h_0().child(
                    super::overview::page_column(window)
                        .h_full()
                        .min_h_0()
                        // Counted, as the Overview's "Islands · 1".
                        .child(super::overview::page_header(
                            format!("Models · {}", all.len()),
                            "Models this mod ships, and those from the mods it depends on.",
                        ))
                        // Usage as a fact under the heading, and the verbs
                        // the selected model offers: the page read as a
                        // gallery with nothing to do until a click.
                        .when(self.project_ui.model.is_none() || all_unused, |d| {
                            d.child(
                                hint(if all_unused {
                                    "None are on an island yet. Select one to see where it fits and to place it."
                                } else {
                                    "Select a model to see where it is used and to place or remove it."
                                })
                                .mt(px(-12.)),
                            )
                        })
                        .child(toolbar)
                        // Out by the grid's own inset and the cards' padding,
                        // so the tiles line up with the heading and toolbar.
                        .child(div().flex_1().min_h_0().relative().mx(px(-8.)).child(body))
                        .children(detail),
                ),
            )
            .into_any_element()
    }

    fn model_detail(
        &self,
        all: &[Entry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let cfg = self.project_ui.model.clone()?;
        let entry = all.iter().find(|e| e.cfg == cfg)?;
        let ours = !matches!(entry.source, Source::Mod(_));
        let model = ours
            .then(|| port::project_models(&self.project.files))
            .and_then(|m| m.into_iter().find(|m| m.cfg == cfg));
        let used = self.used_by(&cfg);
        let usage = if self.project_ui.usage.is_none() {
            "Checking which islands use it…".to_owned()
        } else if used.is_empty() {
            "Not placed on any project island".to_owned()
        } else {
            format!(
                "Placed on {}",
                used.iter()
                    .map(|p| p.rsplit('/').next().unwrap_or(p).trim_end_matches(".a7m"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let (place_cfg, remove_cfg) = (cfg.clone(), cfg.clone());
        let mut actions = div().flex().items_center().gap_2().child(
            action(
                "Place on island",
                if self.island.is_some() {
                    Tone::Primary
                } else {
                    Tone::Secondary
                },
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.place_model(&place_cfg);
                cx.notify();
            })),
        );
        if ours && used.is_empty() {
            actions = actions.child(confirm_button(
                "remove-model",
                "Remove from the mod",
                "Really remove its files?",
                window,
                cx,
                move |this, _, _| this.remove_model(&remove_cfg),
            ));
        }
        Some(
            div()
                .flex_shrink_0()
                .p_2()
                .flex()
                .flex_col()
                .gap_1()
                .border_t_1()
                .border_color(theme::get().border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(model_name(&cfg).to_owned()),
                        )
                        .child(badge(entry.source.label().to_owned(), entry.source.color())),
                )
                .child(hint(cfg.clone()))
                .child(hint(match &model {
                    Some(m) => format!(
                        "{} file(s), {:.1} MB · {usage}",
                        m.files.len(),
                        m.size as f32 / 1e6
                    ),
                    None => format!("Provided by {} · {usage}", entry.source.label()),
                }))
                .child(actions),
        )
    }
}
