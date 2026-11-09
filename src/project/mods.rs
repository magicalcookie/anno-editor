// Other installed mods: finding them, mounting the project's dependencies
// under it the way the mod loader stacks them, telling which mod a model
// comes from, and flagging models an island takes from a mod the project
// does not depend on.
use crate::editor::Editor;
use anno_formats::{
    modinfo::{self, Installed},
    rda,
};
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    theme,
    widgets::{self, Tone, action, badge, card, card_title, checkbox, empty_state, hint},
};
use std::path::{Path, PathBuf};

/// Where a model an island uses comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Shipped,
    Project,
    /// A dependency's id.
    Mod(String),
    /// Nothing mounted has it.
    Missing,
}

/// Models an island takes from one installed mod the project does not
/// depend on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undeclared {
    pub id: String,
    pub name: String,
    pub models: Vec<String>,
}

impl Editor {
    /// Lists the installed mods, leaving out the project's own copy.
    pub fn scan_mods(&mut self) {
        let Some(install) = &self.install else {
            return;
        };
        let roots = modinfo::roots(self.game, install.dir());
        let own = self.project.mod_id().to_owned();
        self.mods = modinfo::installed(self.game, &roots)
            .into_iter()
            .filter(|m| m.info.id != own)
            .collect();
        // By the name the list shows, ignoring case: in folder order
        // "Anno 2070 content for 117" came last.
        self.mods.sort_by_cached_key(|m| {
            let name = if m.info.name.is_empty() {
                &m.info.id
            } else {
                &m.info.name
            };
            name.to_lowercase()
        });
    }

    /// The installed mods the project requires, in its order.
    pub fn dependencies(&self) -> Vec<&Installed> {
        (self.project.info.requires.iter())
            .filter_map(|id| self.mods.iter().find(|m| m.info.id == *id))
            .collect()
    }

    /// Required ids no installed mod has.
    pub fn missing_dependencies(&self) -> Vec<String> {
        (self.project.info.requires.iter())
            .filter(|id| !self.mods.iter().any(|m| m.info.id == **id))
            .cloned()
            .collect()
    }

    /// Serves the dependencies' loose files and then the project's over the
    /// install, as the game's mod loader does, so islands made here open
    /// like shipped ones and can use models from the mods they require.
    pub fn mount_project(&mut self) {
        let Some(install) = &self.install else {
            return;
        };
        let deps: Vec<(String, std::path::PathBuf)> = self
            .dependencies()
            .into_iter()
            .map(|m| (m.info.id.clone(), m.dir.clone()))
            .collect();
        let folder = self.project.folder();
        let mut dirs: Vec<&Path> = deps.iter().map(|(_, d)| d.as_path()).collect();
        dirs.push(&folder);
        if let Err(e) = install.mount_mods(&dirs) {
            eprintln!("Mod folders not mounted: {e:#}");
        }
        self.mounted = deps.into_iter().map(|(id, _)| id).collect();
    }

    pub fn origin(&self, path: &str) -> Origin {
        let Some(install) = &self.install else {
            return Origin::Missing;
        };
        match install.mod_layer(path) {
            Some(layer) => match self.mounted.get(layer) {
                Some(id) => Origin::Mod(id.clone()),
                None => Origin::Project,
            },
            None if install.file(path).is_some() => Origin::Shipped,
            None => Origin::Missing,
        }
    }

    /// Models of the open island that nothing mounted has, grouped by the
    /// installed mod that does. Models no installed mod has are left out;
    /// the island shows them as missing anyway.
    pub fn undeclared_models(&self) -> Vec<Undeclared> {
        let Some(island) = &self.island else {
            return Vec::new();
        };
        let missing: Vec<String> = island
            .model_paths()
            .into_iter()
            .filter(|p| self.origin(p) == Origin::Missing)
            .collect();
        if missing.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Undeclared> = Vec::new();
        for m in self
            .mods
            .iter()
            .filter(|m| !self.mounted.contains(&m.info.id))
        {
            let files = data_files(&m.dir);
            let models: Vec<String> = (missing.iter())
                .filter(|p| files.binary_search(p).is_ok())
                .filter(|p| !out.iter().any(|u| u.models.contains(p)))
                .cloned()
                .collect();
            if !models.is_empty() {
                out.push(Undeclared {
                    id: m.info.id.clone(),
                    name: m.info.name.clone(),
                    models,
                });
            }
        }
        out
    }

    /// Makes the project require mod `id`, writes the manifest and mounts
    /// the mod, so its models load.
    pub fn add_dependency(&mut self, id: &str) {
        if !self.project.info.requires.iter().any(|r| r == id) {
            self.project.info.requires.push(id.to_owned());
        }
        self.finish_dependency_edit(format!("Now requires {id}"));
    }

    pub fn remove_dependency(&mut self, id: &str) {
        self.project.info.requires.retain(|r| r != id);
        self.finish_dependency_edit(format!("No longer requires {id}"));
    }

    fn finish_dependency_edit(&mut self, done: String) {
        let written = self.project.write();
        self.mount_project();
        self.undeclared = self.undeclared_models();
        match written {
            // Models already drawn as missing load on the next open.
            Ok(_) if self.island.is_some() => self.message(
                format!("{done}. Reopen the island to load the mod’s models"),
                false,
            ),
            Ok(_) => self.message(done, false),
            Err(e) => self.message(
                format!("{done}, but modinfo.json was not written: {e:#}"),
                true,
            ),
        }
    }
}

impl Editor {
    /// The folder Install copies into; see `modinfo::install_root`.
    pub(crate) fn mods_root(&self) -> Option<PathBuf> {
        let install = self.install.as_ref()?;
        Some(modinfo::install_root(self.game, install.dir()))
    }

    /// Opens the mods folder in the file manager, making it first: a fresh
    /// install has none.
    pub(crate) fn reveal_mods_folder(&self, cx: &mut Context<Self>) {
        if let Some(dir) = self.mods_root() {
            let _ = std::fs::create_dir_all(&dir);
            cx.reveal_path(&dir);
        }
    }

    /// Rescans when the window comes back into focus; says so only when
    /// the installed mods changed.
    pub(crate) fn rescan_mods_on_return(&mut self, cx: &mut Context<Self>) {
        let ids = |mods: &[Installed]| mods.iter().map(|m| m.info.id.clone()).collect::<Vec<_>>();
        let before = ids(&self.mods);
        self.scan_mods();
        if ids(&self.mods) != before {
            self.mount_project();
            self.message(format!("{} installed mods", self.mods.len()), false);
            cx.notify();
        }
    }

    /// A secondary button: a real action, and links only jump to a tab.
    /// Switching back to the editor rescans by itself; this is for a mod
    /// copied in while the editor had focus.
    fn rescan_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        action("Rescan now", Tone::Secondary).on_click(cx.listener(|this, _, _, cx| {
            this.scan_mods();
            this.mount_project();
            this.message(format!("{} installed mods", this.mods.len()), false);
            cx.notify();
        }))
    }

    pub fn render_project_mods(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.install.is_none() {
            return empty_state("Indexing the install…").into_any_element();
        }
        // Nothing installed and nothing to fix: one card in the Overview's
        // column, so switching tabs does not move the content to the middle
        // of the window.
        if self.mods.is_empty()
            && self.undeclared.is_empty()
            && self.missing_dependencies().is_empty()
        {
            // Unframed, a fixed step under the page's heading: centred in
            // the page it floated far from its title and read as a second
            // screen; in a card pinned top left it left the page blank.
            let body = div().w_full().pt(px(96.)).flex().justify_center().child(
                widgets::empty_state_icon(
                    gpui_editor_kit::icon::Icon::FOLDER,
                    "No other installed mods found",
                )
                .bg(transparent_black())
                // How to get one here; ticking is said over the list
                // once there is one to tick.
                // Two short lines, as the other empty states: one 650 px
                // sentence was slow to read.
                .child(hint(
                    "Unzip each mod into its own folder in the mods folder:",
                ))
                .child(
                    // The body font: monospace cells read as letter-spacing
                    // ("modinfo . json") at this size.
                    hint("For example mods/MyMod/modinfo.json"),
                )
                .child(hint("The list refreshes by itself."))
                .child(
                    div().pt_2().flex().gap_2().child(
                        action("Open mods folder", Tone::Primary)
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_mods_folder(cx))),
                    ), // No Rescan here: the list refreshes when the
                       // editor comes back, as the line above says; the
                       // populated list keeps the button for a retry.
                )
                // Where that folder is, without opening it, in the
                // launcher's form ("Steam library › … › mods"); the full
                // path on hover.
                .when_some(self.mods_root(), |d, dir| {
                    let full = dir.display().to_string();
                    let short = crate::project::short_path(&dir).replace('/', " › ");
                    let shown = if full.contains("/steamapps/common/") {
                        format!("Steam library › {short}")
                    } else {
                        short
                    };
                    d.child(
                        hint(shown)
                            .id("mods-folder-path")
                            .pt_1()
                            .max_w(px(640.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .tooltip(widgets::Tooltip::text("Mods folder", "", full)),
                    )
                }),
            );
            let page = super::overview::page(window).child(
                // The heading where the Overview's starts, so switching tabs
                // does not move it; the card spans the column with its
                // contents centred, as the Overview's banner spans it.
                super::overview::page_column(window)
                    .child(super::overview::page_header("Dependencies", "Mods this one depends on. The game loads them first; their models appear under Models."))
                    .child(body),
            );
            let handle = self.scrolls.get("project-mods");
            return widgets::vscroll("project-mods", &handle, page)
                .size_full()
                .bg(theme::get().panel)
                .into_any_element();
        }
        // Where they all live, said once in the title when it is one place:
        // "game’s mods folder" on every row was noise.
        let one_source = self
            .mods
            .first()
            .map(|m| m.source)
            .filter(|s| self.mods.iter().all(|m| m.source == *s));
        let required = self
            .mods
            .iter()
            .filter(|m| self.project.info.requires.contains(&m.info.id))
            .count();
        // The cards' "Islands · 1" pattern, with how many are ticked.
        let title = format!(
            "Installed mods · {} · {} required",
            crate::project::count(self.mods.len()),
            crate::project::count(required)
        );
        let mut body = card()
            .child(card_title(
                title,
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        action("Open game mods folder", Tone::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_mods_folder(cx))),
                    )
                    .child(self.rescan_button(cx)),
            ))
            // Nothing to tick: the empty line below says how to get mods.
            // What loading first means is the page subtitle's; here only
            // what ticking does, and how many are ticked.
            .when(!self.mods.is_empty(), |d| {
                d.child(hint(format!(
                    "{}Tick the ones this mod needs; players must install them too.",
                    one_source.map_or(String::new(), |s| {
                        let place = s.label();
                        // "In the game’s mods folder." / "In Documents."
                        format!(
                            "In {}{place}. ",
                            if place.starts_with("game") {
                                "the "
                            } else {
                                ""
                            }
                        )
                    })
                )))
            });
        for u in self.undeclared.clone() {
            let id = u.id.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("mods-undeclared-{}", u.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::get().highlight)
                    .child(div().flex_1().text_size(px(11.)).child(format!(
                        "The open island uses {} from {}, which this mod does not require",
                        crate::project::plural(u.models.len(), "model", "models"),
                        u.name
                    )))
                    .child(action("Require it", Tone::Primary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.add_dependency(&id);
                            cx.notify();
                        },
                    ))),
            );
        }
        for id in self.missing_dependencies() {
            let remove = id.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("mods-missing-{id}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(badge("not installed", theme::get().error))
                    .child(div().flex_1().text_size(px(12.)).child(id.clone()))
                    .child(action("Remove", Tone::Secondary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.remove_dependency(&remove);
                            cx.notify();
                        },
                    ))),
            );
        }
        if self.mods.is_empty() {
            body = body.child(hint(
                "No other installed mods found. Put one in the mods folder; it shows up when you switch back.",
            ));
        }
        for (n, m) in self.mods.iter().enumerate() {
            let required = self.project.info.requires.contains(&m.info.id);
            let id = m.info.id.clone();
            let name = if m.info.name.is_empty() {
                m.info.id.clone()
            } else {
                m.info.name.clone()
            };
            // One line: the name, then id and version muted at the right.
            // The id only where it differs from the name ("deep-probe"
            // twice read as a stutter); the folder only when the mods
            // come from more than one.
            let mut facts: Vec<String> = Vec::new();
            if m.info.id != name {
                facts.push(m.info.id.clone());
            }
            if one_source.is_none() {
                facts.push(m.source.label().to_owned());
            }
            if !m.info.version.is_empty() {
                facts.push(m.info.version.clone());
            }
            if !m.enabled {
                facts.push("off in the loader".to_owned());
            }
            body = body.child(
                // The whole row toggles, not only the box and its name.
                // At most 760 px: across the full card the id and version
                // sat 1,300 px from their name. No inset, so the boxes line
                // up with the hint above.
                checkbox(name, required, facts.join(" · "))
                    .id(("installed-mod", n))
                    .max_w(px(760.))
                    .rounded_sm()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.project.info.requires.contains(&id) {
                            this.remove_dependency(&id);
                        } else {
                            this.add_dependency(&id);
                        }
                        cx.notify();
                    })),
            );
        }
        // The Overview's column, so the three tabs line up.
        // The Overview's column and the same page header as the empty
        // state, so the heading stays put whether or not mods are there.
        let page = super::overview::page(window).child(
            super::overview::page_column(window)
                .child(super::overview::page_header(
                    "Dependencies",
                    "Mods this one depends on. The game loads them first; their models appear under Models.",
                ))
                .child(body),
        );
        let handle = self.scrolls.get("project-mods");
        widgets::vscroll("project-mods", &handle, page)
            .size_full()
            .bg(theme::get().panel)
            .into_any_element()
    }
}

/// Normalized paths under a mod's `data/`, sorted.
fn data_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join("data")];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                out.push(rda::normalize_path(&rel).into_owned());
            }
        }
    }
    out.sort_unstable();
    out
}
