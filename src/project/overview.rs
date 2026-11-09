// The Project workspace's Overview pane and the project lifecycle: mod
// details from modinfo.json, install state, the project's islands and other
// files, switching between projects, and the New project dialog.
use super::Project;
use crate::editor::{Editor, Leave, Workspace};
use crate::island::{file_name, island_path};
use anno_formats::modinfo::{self, DIFFICULTIES, id_from_name, valid_id};
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{
        self, Tone, Tooltip, action, badge, card, card_title, confirm_button, field_note,
        field_row, hint, segmented,
    },
};
use std::path::{Path, PathBuf};

/// Other staged files listed by name; the rest are counted.
const LISTED_FILES: usize = 12;

/// A mod detail being typed into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Id,
    Version,
    Creator,
    Description,
    Category,
    /// First GUID of the reserved range.
    GuidBase,
    /// New name of the project island at this index.
    Island(usize),
    /// The Models page's search; filters as it is typed, saves nothing.
    ModelSearch,
}

/// Which field of the New project dialog takes keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewField {
    Name,
    Id,
}

pub struct NewProject {
    name: TextEdit<()>,
    id: TextEdit<()>,
    /// The id follows the name until typed into.
    id_typed: bool,
    typing: NewField,
    /// The new mod folder goes in here, named after the id.
    parent: PathBuf,
}

impl NewProject {
    /// The field keys go to, for platform text input.
    pub(crate) fn typing(&mut self) -> &mut dyn gpui_editor_kit::field::InputBuffer {
        match self.typing {
            NewField::Name => &mut self.name,
            NewField::Id => &mut self.id,
        }
    }
}

/// Project workspace state kept on the editor.
#[derive(Default)]
pub struct ProjectUi {
    pub editing: Option<TextEdit<Field>>,
    pub new_project: Option<NewProject>,
    /// Models pane: the selected cfg and the source shown.
    pub model: Option<String>,
    pub model_filter: super::models::Filter,
    /// Which project islands place each model, once scanned.
    pub usage: Option<std::sync::Arc<std::collections::BTreeMap<String, Vec<String>>>>,
    pub usage_loading: bool,
    /// The game's copy, as of the last save, install or project switch
    /// (`refresh_installed`); render reads it instead of the disk.
    pub installed: Installed,
    /// The Models page's search, once applied (while typed, the edit's text
    /// filters).
    pub model_query: String,
    /// The Models page lists only this mod's models no island places.
    pub models_unused_only: bool,
    /// While the game's copy is older: the files an update would change,
    /// so "outdated" says what the game is missing.
    pub install_changes: Vec<(String, super::Change)>,
}

/// How the game's copy of the mod relates to the project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Installed {
    #[default]
    No,
    Current,
    /// Saved since it was installed.
    Older,
    /// The folder there belongs to another mod.
    Other,
}

/// A card title with its count once there is something: three bold "· 0"
/// on a new project's front page read as a database view.
fn counted(title: &str, n: usize) -> String {
    if n == 0 {
        title.to_owned()
    } else {
        format!("{title} · {}", super::count(n))
    }
}

/// What the mod id is for, the same words in the details card and the New
/// project dialog; each adds what matters there.
const MOD_ID_HELP: &str = "Folder name, and the id other mods depend on.";

impl Editor {
    /// Switches to the project in `folder`. Callers go through `leave` so
    /// unsaved edits are not lost.
    pub fn open_project(&mut self, folder: &Path) {
        match Project::open(folder, self.game) {
            Ok(project) => {
                self.island = None;
                self.ui = None;
                self.pending_save = false;
                self.project = project;
                self.project_ui = ProjectUi::default();
                self.undeclared.clear();
                super::remember(&self.config, folder);
                self.scan_mods();
                self.mount_project();
                self.refresh_installed();
                let missing = self.missing_dependencies();
                if missing.is_empty() {
                    self.message(
                        format!("Opened {} ({})", self.project.info.name, folder.display()),
                        false,
                    );
                } else {
                    self.message(
                        format!(
                            "Opened {}. Required mods not installed: {}",
                            self.project.info.name,
                            missing.join(", ")
                        ),
                        true,
                    );
                }
                self.switch(Workspace::Project);
            }
            Err(e) => self.message(format!("Not opened: {e:#}"), true),
        }
    }

    /// File → Open mod folder…
    pub fn pick_project_folder(&mut self, cx: &mut Context<Self>) {
        self.pick_path("Open a mod folder", true, cx, |this, path, cx| {
            this.leave(Leave::Project(path), cx)
        });
    }

    /// A file (or folder) picker whose choice goes to `then`.
    pub fn pick_path(
        &mut self,
        prompt: &'static str,
        directories: bool,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Context<Self>) + 'static,
    ) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
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
                then(this, path, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// File → New project…
    pub fn open_new_project(&mut self, _: &mut Context<Self>) {
        // The dialog takes the keys: a half-typed value is applied first.
        self.end_typing();
        self.project_ui.new_project = Some(NewProject {
            name: TextEdit::new((), "", Filter::Text),
            id: TextEdit::new((), "", Filter::Text),
            id_typed: false,
            typing: NewField::Name,
            parent: self.config.join("mods"),
        });
    }

    fn create_project(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &self.project_ui.new_project else {
            return;
        };
        if d.name.text.trim().is_empty() {
            return self.message("Give the project a name first", true);
        }
        let (name, id) = (d.name.text.trim().to_owned(), d.id.text.trim().to_owned());
        let folder = d.parent.join(&id);
        match Project::create(&folder, &name, &id, self.game) {
            Ok(_) => {
                self.project_ui.new_project = None;
                self.leave(Leave::Project(folder), cx);
            }
            Err(e) => self.message(format!("Not created: {e:#}"), true),
        }
    }

    /// Keys for the New project dialog and the Overview's fields: Enter
    /// applies, Escape backs out, Tab moves between the dialog's fields.
    pub fn project_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if let Some(d) = &mut self.project_ui.new_project {
            match key {
                "escape" => self.project_ui.new_project = None,
                "enter" => self.create_project(cx),
                "tab" => {
                    d.typing = match d.typing {
                        NewField::Name => NewField::Id,
                        NewField::Id => NewField::Name,
                    }
                }
                _ => {
                    let edit = match d.typing {
                        NewField::Name => &mut d.name,
                        NewField::Id => &mut d.id,
                    };
                    if edit.key_down_in(event, cx) == EditEvent::Ignored {
                        return;
                    }
                    if d.typing == NewField::Id {
                        d.id_typed = true;
                    } else if !d.id_typed {
                        d.id = TextEdit::new((), id_from_name(&d.name.text), Filter::Text);
                    }
                }
            }
            cx.notify();
            return;
        }
        let Some(edit) = &mut self.project_ui.editing else {
            return;
        };
        match edit.key_down_in(event, cx) {
            EditEvent::Commit => {
                let edit = self.project_ui.editing.take().expect("an edit");
                self.apply_field(edit.field, edit.text.trim());
            }
            EditEvent::Cancel => self.project_ui.editing = None,
            EditEvent::Changed | EditEvent::Ignored => {}
        }
        cx.notify();
    }

    fn apply_field(&mut self, field: Field, text: &str) {
        let info = &mut self.project.info;
        match field {
            Field::Name if text.is_empty() => {
                return self.message("The project needs a name", true);
            }
            Field::Name => info.name = text.to_owned(),
            Field::Version => {
                let parts: Vec<&str> = text.split('.').collect();
                if parts.len() != 3 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
                    return self.message("The version is three numbers: MAJOR.minor.patch", true);
                }
                info.version = text.to_owned();
            }
            Field::Creator => info.creator = text.to_owned(),
            Field::Description => info.description = text.to_owned(),
            Field::Category => info.category = text.to_owned(),
            Field::GuidBase => match super::parse_guid_base(text) {
                Some(base) => self.project.set_guid_base(base),
                None => {
                    return self.message(
                            "The range start is the first GUID of your reserved range, a number like 1,938,924,000",
                            true,
                        );
                }
            },
            Field::Id => return self.change_mod_id(text),
            Field::Island(index) => return self.rename_island(index, text),
            Field::ModelSearch => {
                self.project_ui.model_query = text.to_owned();
                return;
            }
        }
        self.finish_project_edit("Saved the mod details".into());
    }

    fn change_mod_id(&mut self, id: &str) {
        let old = self.project.mod_id().to_owned();
        if id == old {
            return;
        }
        if !valid_id(id) {
            return self.message(
                "A mod id is letters, digits, '-', '_' and '.', not starting with '.' or '-'",
                true,
            );
        }
        match self.project.set_mod_id(id) {
            Ok(folder) => {
                super::remember(&self.config, &folder);
                self.mount_project();
                // An installed copy under the old id would load as a second
                // mod with the same islands; removing it is the user's call.
                let stale = (self.install.as_ref()).is_some_and(|i| {
                    [
                        modinfo::install_root(self.game, i.dir()),
                        i.dir().join("mods"),
                    ]
                    .iter()
                    .any(|root| root.join(&old).exists())
                });
                self.message(
                    if stale {
                        format!(
                            "The mod id is now {id}. The game still has mods/{old}; uninstall it from the game before installing again, or both load"
                        )
                    } else {
                        format!("The mod id is now {id}")
                    },
                    stale,
                );
            }
            Err(e) => self.message(format!("Mod id not changed: {e:#}"), true),
        }
    }

    fn rename_island(&mut self, index: usize, name: &str) {
        let Some(entry) = self.project.islands.get(index).cloned() else {
            return;
        };
        if file_name(name).trim_matches('_').is_empty() {
            return self.message("The island needs a name", true);
        }
        let open = self.island.as_ref().is_some_and(|i| i.path == entry.path);
        if open && self.island.as_ref().is_some_and(|i| i.dirty()) {
            return self.message("Save the island (Ctrl S) before renaming it", true);
        }
        let to = island_path(self.game, &entry.region.to_ascii_lowercase(), name);
        if self.install.as_ref().is_some_and(|i| i.file(&to).is_some()) {
            return self.message(format!("{to} is a shipped file; pick another name"), true);
        }
        if let Err(e) = self.project.rename_island(&entry.path, &to) {
            return self.message(format!("Not renamed: {e:#}"), true);
        }
        if open && let Some(i) = &mut self.island {
            i.path = to.clone();
        }
        self.rebuild_test_maps();
        self.finish_project_edit(format!("Renamed to {to}"));
    }

    fn delete_island(&mut self, index: usize) {
        let Some(entry) = self.project.islands.get(index).cloned() else {
            return;
        };
        if self.island.as_ref().is_some_and(|i| i.path == entry.path) {
            self.island = None;
        }
        let files = self.project.remove_island(&entry.path);
        self.rebuild_test_maps();
        self.finish_project_edit(format!("Deleted {} and its {files} file(s)", entry.path));
    }

    /// Test maps name island paths; replan them after islands move or go.
    fn rebuild_test_maps(&mut self) {
        if self.project.maps.is_empty() {
            return;
        }
        let plans = match &self.install {
            Some(install) if self.project.islands.iter().any(|i| i.listing.on_test_map) => {
                crate::playtest::plan(install, self.assets.as_deref(), &self.project).ok()
            }
            _ => None,
        };
        match plans {
            Some(plans) => crate::playtest::apply(plans, &mut self.project),
            None => self.project.set_maps(Vec::new()),
        }
    }

    /// Writes the project after an edit and says what changed.
    pub fn finish_project_edit(&mut self, done: String) {
        let written = self.project.write();
        self.mount_project();
        self.project_ui.usage = None;
        match written {
            Ok(_)
                if {
                    self.refresh_installed();
                    self.project_ui.installed == Installed::Older
                } =>
            {
                self.message(
                    format!("{done}. The game has the old version until you install again"),
                    false,
                )
            }
            Ok(_) => self.message(done, false),
            Err(e) => self.message(
                format!("{done}, but the project was not written: {e:#}"),
                true,
            ),
        }
    }

    fn uninstall_mod(&mut self) {
        let Some(install) = &self.install else {
            return self.message("No install is open", true);
        };
        match self.project.uninstall(install.dir(), self.project.mod_id()) {
            Ok(true) => {
                self.refresh_installed();
                self.message(
                    format!("Removed mods/{} from the game", self.project.mod_id()),
                    false,
                )
            }
            Ok(false) => self.message("The mod is not installed", false),
            Err(e) => self.message(format!("Not uninstalled: {e:#}"), true),
        }
    }

    /// Reads the game's copy again; see `ProjectUi::installed`.
    pub fn refresh_installed(&mut self) {
        self.project_ui.installed = self.check_installed();
        // Compared by content on save, install and project switch: mods are
        // tens of files, so this stays well under a frame's worth of work.
        self.project_ui.install_changes = match (self.installed_copy(), self.project_ui.installed) {
            (Some(copy), Installed::Older) => {
                super::changes_since(&self.project.folder(), &copy, 50)
            }
            _ => Vec::new(),
        };
    }

    /// What an update brings, one line per file, for the banner and the
    /// Update button's tooltip: "Changed islands/cove.a7m".
    pub fn install_change_lines(&self, max: usize) -> Vec<String> {
        let changes = &self.project_ui.install_changes;
        let mut lines: Vec<String> = changes
            .iter()
            .take(max)
            .map(|(path, change)| {
                // What the update does to the game's copy: "Removed" read
                // as already gone.
                let verb = match change {
                    super::Change::Added => "Adds",
                    super::Change::Changed => "Updates",
                    super::Change::Removed => "Removes",
                };
                format!("{verb} {path}")
            })
            .collect();
        if changes.len() > max {
            lines.push(format!("and {} more", changes.len() - max));
        }
        lines
    }

    /// The update in one line: "1 to update · 49 to remove from the game's
    /// copy". The list behind it folds away; five raw paths pushed the
    /// page down by 140 px.
    pub fn install_change_summary(&self) -> Option<String> {
        let changes = &self.project_ui.install_changes;
        if changes.is_empty() {
            return None;
        }
        let n = |c| changes.iter().filter(|(_, k)| *k == c).count();
        let mut parts = Vec::new();
        for (count, word) in [
            (n(super::Change::Changed), "to update"),
            (n(super::Change::Added), "to add"),
            (n(super::Change::Removed), "to remove from the game’s copy"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {word}"));
            }
        }
        Some(parts.join(" · "))
    }

    /// The one install action's label, which follows the game's copy.
    pub fn install_label(&self) -> &'static str {
        match self.project_ui.installed {
            Installed::Older => "Update game copy",
            Installed::Current => "Installed ✓",
            _ => "Install",
        }
    }

    /// The game's copy of this mod: in the install root, or in
    /// `<install>/mods` where installs went before. The next install moves
    /// an old copy over.
    fn installed_copy(&self) -> Option<PathBuf> {
        let install = self.install.as_ref()?;
        [
            modinfo::install_root(self.game, install.dir()),
            install.dir().join("mods"),
        ]
        .into_iter()
        .map(|root| root.join(self.project.mod_id()))
        .find(|copy| copy.join("modinfo.json").exists())
    }

    fn check_installed(&self) -> Installed {
        let Some(copy) = self.installed_copy() else {
            return Installed::No;
        };
        if !self.project.owns_install(&copy) {
            return Installed::Other;
        }
        // Copies get the time of the copy and saves rewrite the manifest,
        // so a manifest newer than the game's means a save since.
        let time = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match (
            time(self.project.folder().join("modinfo.json")),
            time(copy.join("modinfo.json")),
        ) {
            (Some(saved), Some(copied)) if saved > copied => Installed::Older,
            _ => Installed::Current,
        }
    }

    /// A one-line text field for `field` showing `value`; a click edits it.
    pub(super) fn detail_field(
        &self,
        field: Field,
        value: &str,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let editing = (self.project_ui.editing.as_ref())
            .filter(|e| e.field == field)
            .map(|e| e.display());
        let start = value.to_owned();
        let id = SharedString::from(format!("project-field-{field:?}"));
        // The description is prose: three wrapping lines at full width.
        // Short values need no 700 px bar.
        let input = if field == Field::Description {
            gpui_editor_kit::field::text_area(id, value, editing, placeholder, 3)
        } else {
            // Two widths: the short values (a version, a category, a GUID)
            // as wide as they get, the rest the column's width, so the card
            // keeps one right edge; four widths left it ragged.
            let short = match field {
                Field::Version | Field::GuidBase | Field::Category => Some(180.),
                // The Library's search width.
                Field::ModelSearch => Some(260.),
                _ => None,
            };
            match short {
                Some(w) => text_field(id, value, editing, placeholder)
                    .w(px(w))
                    .max_w_full(),
                None => text_field(id, value, editing, placeholder).w_full(),
            }
        };
        input.on_click(cx.listener(move |this, _, window, cx| {
            this.focus.focus(window, cx);
            // Moving to another field keeps what was typed in this one, as
            // Enter would (invalid text still says why); replacing the
            // draft lost ordinary form input. A click into the open field
            // leaves its draft alone.
            match this.project_ui.editing.take() {
                Some(open) if open.field == field => {
                    this.project_ui.editing = Some(open);
                    return;
                }
                Some(open) => this.apply_field(open.field, open.text.trim()),
                None => {}
            }
            this.project_ui.editing = Some(TextEdit::new(field, start.clone(), Filter::Text));
            cx.notify();
        }))
    }

    pub fn render_project_overview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The islands' map pictures for their rows, as the Islands pane has.
        if self.thumbs.is_none()
            && let Some(install) = self.install.clone()
        {
            let dir = self.config.join("thumbnails");
            self.thumbs = Some(crate::island::catalog::Thumbs::start(install, dir));
        }
        if let Some(thumbs) = &mut self.thumbs {
            thumbs.request(self.project.islands.iter().map(|i| i.path.as_str()));
        }
        self.poll_thumbs(cx);
        let info = self.project.info.clone();
        let installed = self.project_ui.installed;

        let header = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(info.name.clone()),
                    )
                    // The game this project belongs to: a tinted tag, not
                    // an outline (read as a focused field) nor a grey
                    // fill (read as disabled).
                    .child(
                        div()
                            .px_2()
                            .py(px(1.))
                            .rounded(px(3.))
                            // A muted filled tag: in link blue it read as a
                            // link, with a border as a button.
                            .bg(theme::get().secondary)
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            .child(self.game.profile().title),
                    )
                    .child(div().flex_1()),
            )
            // The reveal as a link after the path it opens: a lone header
            // button sat far from it and competed with the banner's action.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(hint(format!(
                        "v{} · {} ·",
                        info.version,
                        super::short_path(&self.project.folder())
                    )))
                    .child(
                        div()
                            .id("overview-reveal")
                            .text_size(px(11.))
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child("Show in file manager")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.reveal_path(&this.project.folder().join("modinfo.json"))
                            })),
                    ),
            );

        // Where the game's copy stands; the top bar's chip holds the button.
        let (text, color) = match installed {
            Installed::No => (
                "Not in the game yet. Install copies the mod into the mods folder.",
                theme::get().border,
            ),
            // "Before testing" only while a test is possible; otherwise it
            // promised a step the disabled Test button then refused.
            Installed::Older if self.playtest_blocker().is_none() => (
                // Test in game installs the mod itself: "update before
                // testing" read as two steps.
                // The top bar's words as the headline; the summary under it
                // says what an update does.
                // The headline alone; the summary under it says what an
                // update does, and Test in game's tooltip that it updates too.
                "Game copy is out of date",
                theme::get().highlight,
            ),
            Installed::Older => (
                "An older copy of this mod is installed in the game. Update it to see your changes there.",
                theme::get().highlight,
            ),
            Installed::Current => (
                "The game has the current version; it loads on the next start.",
                theme::get().ok,
            ),
            Installed::Other => (
                "Another mod in the game uses this mod id. Change the id below before installing.",
                theme::get().error,
            ),
        };
        // One row: what the game has, then what to do about it.
        // A routine state: a neutral card with the state's colour as a
        // leading stripe; all amber, it outweighed everything on the page.
        let status = card()
            .relative()
            .overflow_hidden()
            .border_color(theme::get().border)
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.))
                    .bg(color),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    // The test prerequisite on its own line under the state, not
                    // beside the Update button where it read as its caption.
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            // A heading's weight while there is something to
                            // do: at body weight the page's most important
                            // status read as a caption.
                            .child(
                                div()
                                    .text_size(px(if installed == Installed::Older {
                                        13.
                                    } else {
                                        12.
                                    }))
                                    .when(installed == Installed::Older, |d| {
                                        d.font_weight(FontWeight::SEMIBOLD)
                                    })
                                    .child(text),
                            )
                            // What the game is missing, so "older" says what
                            // an update brings. The test-map prerequisite
                            // lives in the Islands card, beside its islands.
                            .when_some(
                                (installed == Installed::Older)
                                    .then(|| self.install_change_summary())
                                    .flatten(),
                                |d, summary| {
                                    let (fold, open) = widgets::disclosure_link(
                                        "overview-install-files",
                                        "Show files",
                                        "Hide files",
                                        window,
                                        cx,
                                    );
                                    d.child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(theme::get().muted)
                                                    .child(summary),
                                            )
                                            .child(fold.text_size(px(11.))),
                                    )
                                    .when(open, |d| {
                                        d.children(self.install_change_lines(50).into_iter().map(
                                            |line| {
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(theme::get().muted)
                                                    .font_family(theme::MONO_FONT)
                                                    .child(line)
                                            },
                                        ))
                                    })
                                },
                            ),
                    )
                    // Disabled with its reason until there is an island to test.
                    // With nothing to test, the prerequisite in words where the
                    // button would be, not only in a disabled button's tooltip.
                    .child(match self.playtest_blocker() {
                        // Said under the state line instead.
                        Some(_) if self.project.islands.iter().all(|i| !i.listing.on_test_map) => {
                            div().into_any_element()
                        }
                        Some(why) => widgets::action_off("Test in game…", why).into_any_element(),
                        // Says the update it brings while the copy is old,
                        // where only its tooltip did.
                        None => action(
                            if installed == Installed::Older {
                                "Update & test in game…"
                            } else {
                                "Test in game…"
                            },
                            Tone::Secondary,
                        )
                        .tooltip(Tooltip::text(
                            "Test in game",
                            "",
                            "Updates the game copy, then starts the game on the test map",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_playtest();
                            cx.notify();
                        }))
                        .into_any_element(),
                    })
                    // The warning resolves where it is read: the same action as
                    // the top bar's chip.
                    .when(matches!(installed, Installed::No | Installed::Older), |d| {
                        d.child(
                            action(
                                if installed == Installed::No {
                                    "Install"
                                } else {
                                    "Update game copy"
                                },
                                // While there is no island, New island leads:
                                // an update then has little new to show.
                                if self.project.islands.is_empty() {
                                    Tone::Secondary
                                } else {
                                    Tone::Primary
                                },
                            )
                            // Secondary, it still wears the banner's amber: it is
                            // the action the banner asks for.
                            .when(self.project.islands.is_empty(), |b| {
                                b.border_color(theme::get().highlight)
                                    .text_color(theme::get().highlight)
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.install_mod();
                                cx.notify();
                            })),
                        )
                    }),
            );

        let mut details = card()
            .child(card_title("Mod details", div()))
            .child(field_row(
                "Name",
                self.detail_field(Field::Name, &info.name, "Shown in the mod list", cx),
            ))
            .child(field_row(
                "Mod id",
                self.detail_field(Field::Id, &info.id, "name-creator", cx),
            ))
            // One line under the field; why it is fixed goes on hover.
            .child(
                field_note(format!("{MOD_ID_HELP} Keep it once published."))
                    .id("mod-id-help")
                    .tooltip(widgets::Tooltip::text(
                        "Mod id",
                        "",
                        "Mods that depend on this one find it by its id: changing it after publishing breaks them",
                    )),
            )
            .child(field_row(
                "Version",
                self.detail_field(Field::Version, &info.version, "1.0.0", cx),
            ))
            .child(field_row(
                "Author",
                self.detail_field(Field::Creator, &info.creator, "Your name", cx),
            ))
            .child(field_row(
                "Description",
                self.detail_field(
                    Field::Description,
                    &info.description,
                    "What the mod adds",
                    cx,
                ),
            ))
            .child(field_row(
                "Category",
                self.detail_field(Field::Category, &info.category, "Misc", cx),
            ));
        if self.game.profile().difficulty {
            details = details
                .child(field_row(
                    "Difficulty",
                    segmented(
                        "project-difficulty",
                        DIFFICULTIES.map(|d| {
                            let mut label = d.to_owned();
                            label[..1].make_ascii_uppercase();
                            (d.to_owned(), label.into())
                        }),
                        &info.difficulty,
                        cx,
                        |this, d, _, _| {
                            this.project.info.difficulty = d;
                            this.finish_project_edit("Saved the mod details".into());
                        },
                    ),
                ))
                // "Unchanged" alone read as a pending edit or an island setting.
                .child(field_note(
                    "Tells players how the mod affects difficulty; mod managers show it.",
                ));
        }
        // Registry setup folded under its own heading: open among the
        // everyday fields it read as one more thing to fill in now. Its
        // heading says whether the range is reserved, so the warning stays
        // in view.
        let (publishing, publishing_open) =
            widgets::collapsible("overview-publishing", "Publishing", false, window, cx);
        let reserved = self.project.guid_reserved();
        details = details.child(
            publishing.child(
                div()
                    .ml_2()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .font_weight(FontWeight::NORMAL)
                    .text_color(theme::get().muted)
                    // Grey, not amber: fine until publishing, and amber is
                    // kept for what blocks playing. A ring, as an open step:
                    // a filled dot read as a one-item list bullet.
                    .when(!reserved, |d| {
                        d.child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .border_1()
                                .border_color(theme::get().muted),
                        )
                    })
                    .child(if reserved {
                        format!(
                            "GUIDs {} – {}",
                            self.project.guid_base(),
                            self.project.guid_base() + 99
                        )
                    } else {
                        "GUIDs not reserved".to_owned()
                    }),
            ),
        );
        if publishing_open {
            details = details.child(field_row(
            "First GUID",
            // The value in the row, as every other field shows its value;
            // the registry is a small action beside it.
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .child(
                    // To the field column's right edge, as every other row.
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        // Editable: the range reserved in the registry is
                        // entered here, so publishing does not end outside.
                        .child(
                            self.detail_field(
                                Field::GuidBase,
                                // Raw digits: a GUID is pasted into XML, so
                                // thousands commas would come along.
                                &self.project.guid_base().to_string(),
                                "first GUID",
                                cx,
                            )
                            .flex_1()
                            .min_w_0(),
                        )
                        .child(
                            action("Reserve GUIDs ↗", Tone::Secondary)
                                .h(px(24.))
                                .text_size(px(11.))
                                .on_click(|_, _, cx| {
                                    cx.open_url("https://github.com/anno-mods/GuidRanges")
                                }),
                        ),
                )
                // Whether the value is a reservation or still the one picked
                // from the mod id: both look like a settled number.
                .child(if self.project.guid_reserved() {
                    hint(format!(
                        "Islands this mod creates are numbered {} – {}; ones already in it keep theirs.",
                        self.project.guid_base(),
                        self.project.guid_base() + 99
                    ))
                } else {
                    // A small amber mark, the words muted: it matters before
                    // publishing, and all-amber text competed with the
                    // update banner as if both were faults.
                    // Full width: the column aligns items to the start, which
                    // sized the row to its unwrapped text, past the card.
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .gap(px(6.))
                        .child(
                            div()
                                .mt(px(6.))
                                .size(px(7.))
                                .flex_shrink_0()
                                .rounded_full()
                                // The heading's ring: one mark for one state.
                                .border_1()
                                .border_color(theme::get().muted),
                        )
                        // flex_1 + min_w_0 so it wraps inside the card; a
                        // bare hint in the row ran past its edge.
                        .child(hint(
                            // The wizard's sentence, word for word: one state, one line.
                            "GUIDs aren’t reserved yet. Fine for testing; reserve before publishing.",
                        ).flex_1().min_w_0())
                }),
        ));
        }

        let missing = self.missing_dependencies();
        let mut requires = card()
            // Counted, its jump to the tab on the heading row as the other
            // cards' ways in are.
            .child(card_title(
                counted("Dependencies", info.requires.len()),
                div()
                    .id("overview-dependencies")
                    .text_size(px(12.))
                    .text_color(theme::get().accent)
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .child("Manage dependencies →")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_pane(crate::editor::Pane::ProjectMods);
                        cx.notify();
                    })),
            ))
            .child(hint(if info.requires.is_empty() {
                "Needs no other mods.".to_owned()
            } else {
                format!("Needs {}.", info.requires.join(", "))
            }));
        if !missing.is_empty() {
            requires = requires.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::get().error)
                    .child(format!("Not installed here: {}", missing.join(", "))),
            );
        }
        for u in self.undeclared.clone() {
            let id = u.id.clone();
            requires = requires.child(
                div()
                    .id(SharedString::from(format!("undeclared-{}", u.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(theme::get().highlight)
                            .child(format!(
                                "The open island uses {} from {}, which this mod does not require.",
                                plural(u.models.len(), "model", "models"),
                                u.name
                            )),
                    )
                    .child(action("Require it", Tone::Primary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.add_dependency(&id);
                            cx.notify();
                        },
                    ))),
            );
        }

        let islands = self.overview_islands(window, cx);
        let others = self.overview_files(window, cx);
        // Two rows of one shape: what it does on the left, the button on
        // the right.
        let danger_row = |title: &'static str, what: &'static str, button: Stateful<Div>| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(12.)).child(title))
                        .child(hint(what)),
                )
                .child(div().w(px(220.)).flex().justify_end().child(button))
        };
        // Folded by default: maintenance should not outweigh making content.
        let (heading, danger_open) =
            widgets::collapsible("project-danger", "Danger zone", false, window, cx);
        // A card like the others in its column, folded or open: as a bare
        // disclosure row under three framed cards it read as stray. The red
        // frame comes with the buttons.
        let mut danger = card()
            .when(danger_open, |c| c.border_color(theme::get().danger_border))
            .child(heading.my_0().border_t_0().text_size(px(13.)));
        if danger_open {
            danger = danger.child(danger_row(
                "Clear the project",
                "Removes every island and edited file from the mod. The mod details stay; the game folder is not touched.",
                confirm_button(
                    "clear-project",
                    "Clear project…",
                    "Drop every island and file?",
                    window,
                    cx,
                    |this, _, _| {
                        this.island = None;
                        this.project.clear();
                        this.finish_project_edit("Cleared the project".into());
                    },
                ),
            ));
        }
        if danger_open && matches!(installed, Installed::Current | Installed::Older) {
            danger = danger.child(danger_row(
                "Uninstall from the game",
                "Deletes the mod’s folder in the game; the project stays.",
                confirm_button(
                    "uninstall",
                    "Uninstall…",
                    "Remove the game’s copy?",
                    window,
                    cx,
                    |this, _, _| this.uninstall_mod(),
                ),
            ));
        }

        // Wide windows get two columns: what the mod contains on the left,
        // how it is published on the right. Narrow ones stack them.
        let wide = window.viewport_size().width > px(1300.);
        let column = || div().flex_1().min_w_0().flex().flex_col().gap_4();
        // Wide: what the mod holds on the left, its details on the right,
        // cards at their own height; maintenance at the foot of the left
        // column. Narrow: one column, maintenance last.
        let content = if wide {
            div().flex().flex_col().gap_4().child(
                // Both columns end on one line once the mod has islands:
                // the Islands card takes the height to Mod details' foot
                // (it holds the island list as it grows); the others fit
                // their content. Empty, it fits its own: stretched, its
                // two buttons and a link sat over a blank band.
                div()
                    .flex()
                    .items_start()
                    .gap_4()
                    .child(
                        // Stretched only once its rows fill it: with one
                        // island the stretch left an empty band under it.
                        column()
                            .when(self.project.islands.len() >= 3, |c| c.self_stretch())
                            .child(if self.project.islands.len() < 3 {
                                islands
                            } else {
                                islands.flex_1()
                            })
                            .child(others)
                            .child(requires)
                            // Under the left column, not page wide below
                            // both: there it sat under a pocket the
                            // shorter column left, and the columns ended
                            // apart.
                            .child(danger),
                    )
                    .child(column().child(details)),
            )
        } else {
            column()
                .child(islands)
                .child(others)
                .child(details)
                .child(requires)
                .child(danger)
        };
        let body = page(window).child(
            page_column(window)
                .child(header)
                .child(status)
                .child(content),
        );
        let handle = self.scrolls.get("project-overview");
        widgets::vscroll("project-overview", &handle, body)
            .size_full()
            .bg(theme::get().panel)
            .into_any_element()
    }

    fn overview_islands(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let mut list = card().child(card_title(
            counted("Islands", self.project.islands.len()),
            div()
                .flex()
                .gap_2()
                // Primary while the project has no island: its first job.
                // With islands, it steps back while the update banner leads.
                .child(
                    action(
                        "New island…",
                        if self.project.islands.is_empty()
                            || !matches!(
                                self.project_ui.installed,
                                Installed::Older | Installed::No
                            )
                        {
                            Tone::Primary
                        } else {
                            Tone::Secondary
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_wizard();
                        cx.notify();
                    })),
                )
                // After New island, the order File → Islands and the Island
                // viewport use.
                .child(
                    action(
                        crate::game_text::of(self.game).import_short,
                        Tone::Secondary,
                    )
                    .tooltip(widgets::Tooltip::text(self.import_label(), "", ""))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_import(cx);
                        cx.notify();
                    })),
                ),
        ));
        if self.project.islands.is_empty() {
            list = list
                .child(hint(
                    "No islands yet. Start a new one, or bring one over from the other game.",
                ))
                // The third start, editing a shipped island, lives in another
                // workspace; a link here says it exists.
                .child(
                    div()
                        .id("overview-browse-islands")
                        .pt_1()
                        .text_size(px(12.))
                        .text_color(theme::get().accent)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child("Or edit one of the game’s islands →")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.island_kind = crate::island::IslandKind::Playable;
                            this.switch(crate::editor::Workspace::Island);
                            this.show_pane(crate::editor::Pane::IslandList);
                            cx.notify();
                        })),
                );
        } else if self.project.islands.iter().all(|i| !i.listing.on_test_map) {
            // Only once there is an island to tick; with none, File → Test
            // in game already says why it waits.
            list = list.child(hint(
                "None is on the test map; tick \"On the test map\" in an island’s settings to use Test in game.",
            ));
        }
        for (index, i) in self.project.islands.iter().enumerate() {
            let name = i
                .path
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".a7m")
                .to_owned();
            let role = super::ROLES
                .iter()
                .find(|(_, k)| *k == i.listing.kind)
                .map_or(i.listing.kind.as_str(), |(label, _)| label);
            let path = i.path.clone();
            let open = self.island.as_ref().is_some_and(|o| o.path == i.path);
            // A list row, as the Islands pane draws one: the island's map, its
            // name (a click opens it) and what it is under the name. Rename
            // is a link that turns the name into its field; an always-open
            // field made the card read as a form to fill in.
            let renaming = self
                .project_ui
                .editing
                .as_ref()
                .is_some_and(|e| e.field == Field::Island(index));
            let picture = self.thumbs.as_ref().and_then(|t| t.get(&i.path));
            let title: AnyElement = if renaming {
                self.detail_field(Field::Island(index), &name, "Island name", cx)
                    .tooltip(Tooltip::text("Rename", "Enter", "Type a new name"))
                    .into_any_element()
            } else {
                let path = path.clone();
                div()
                    .id(("project-island-name", index))
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    // Link blue: the name opens the island too, as the
                    // Islands pane's rows do.
                    .text_color(theme::get().accent)
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .child(name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_island(&path, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            };
            let rename_from = name.clone();
            list = list.child(
                div()
                    .id(("project-island", index))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        // 64 px: at 40 the island read as a dark smudge.
                        div()
                            .size(px(64.))
                            .flex_shrink_0()
                            .rounded(px(4.))
                            .overflow_hidden()
                            .bg(rgb(crate::colors::SEA))
                            .children(picture.map(|p| img(p).size_full())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(title)
                            // "island": bare, "Regular or start" trailed off.
                            .child(hint(format!(
                                "{} · {} · {role} island",
                                crate::island::province_short(&i.region),
                                i.class()
                            )))
                            // Whether Test in game will show it, here where
                            // the islands are listed: before, only the open
                            // island's Inspector said so.
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .text_size(px(11.))
                                    .text_color(theme::get().muted)
                                    .child(
                                        div()
                                            .text_color(if i.listing.on_test_map {
                                                theme::get().success
                                            } else {
                                                theme::get().muted
                                            })
                                            .child(if i.listing.on_test_map {
                                                "✓"
                                            } else {
                                                "○"
                                            }),
                                    )
                                    .child(if i.listing.on_test_map {
                                        "On the test map"
                                    } else {
                                        "Not on the test map"
                                    }),
                            ),
                    )
                    .when(open, |d| d.child(badge("open", theme::get().accent)))
                    // Open first: with the name link the only way in, the
                    // row's two buttons were Rename and Delete, and the
                    // everyday action looked like the least clickable one.
                    // Neutral, as the banner holds the screen's primary.
                    .when(!renaming, |d| {
                        let path = path.clone();
                        d.child(action("Open", Tone::Secondary).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.open_island(&path, cx);
                                cx.notify();
                            },
                        )))
                    })
                    .when(!renaming, |d| {
                        d.child(
                            // A framed button beside Delete: as a blue text
                            // link it read as a second link to the island.
                            action("Rename", Tone::Secondary).on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.focus.focus(window, cx);
                                    this.project_ui.editing = Some(TextEdit::new(
                                        Field::Island(index),
                                        rename_from.clone(),
                                        Filter::Text,
                                    ));
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(confirm_button(
                        format!("delete-island-{index}"),
                        "Delete",
                        "Really delete?",
                        window,
                        cx,
                        move |this, _, _| this.delete_island(index),
                    )),
            );
        }
        list
    }

    /// Files the mod edits that are neither islands nor models: the
    /// Overview's "Other edited files", and the status bar's count of them.
    pub fn other_edited_files(&self) -> Vec<String> {
        let models: Vec<String> = super::models::project_cfg_files(&self.project);
        (self.project.other_files().into_iter())
            .filter(|f| !models.contains(f))
            .collect()
    }

    /// What the mod holds, in the Overview's terms: "1 island · 25 models
    /// · 3 other files". A raw count of written files ("1410 edited game
    /// files") counted each model's meshes and textures, beside an
    /// Overview that said no other files were edited.
    pub fn mod_contents(&self) -> String {
        // Models, not their files: each brings its meshes and textures.
        let models = crate::island::port::project_models(&self.project.files).len();
        let others = self.other_edited_files().len();
        let mut parts = vec![super::plural(
            self.project.islands.len(),
            "island in the mod",
            "islands in the mod",
        )];
        if models > 0 {
            parts.push(super::plural(models, "model", "models"));
        }
        if others > 0 {
            parts.push(super::plural(others, "other file", "other files"));
        }
        parts.join(" · ")
    }

    fn overview_files(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let others = self.other_edited_files();
        // Both ways in live in other workspaces; links say where. Empty, they
        // sit on the heading row, as Islands' buttons do on its: on a line
        // of their own under one sentence the card read as scaffolding.
        let link = |id: &'static str, label: &'static str, to: crate::editor::Workspace| {
            div()
                .id(id)
                .text_size(px(12.))
                .text_color(theme::get().accent)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.switch(to);
                    cx.notify();
                }))
        };
        let ways = div().flex().gap_4().when(others.is_empty(), |d| {
            d.child(link(
                "overview-edit-ui",
                "Edit a UI screen →",
                crate::editor::Workspace::Ui,
            ))
            .child(link(
                "overview-browse-assets",
                "Browse game files →",
                crate::editor::Workspace::Assets,
            ))
        });
        let mut list = card().child(card_title(
            counted("Other edited files", others.len()),
            ways,
        ));
        if others.is_empty() {
            list = list.child(hint(
                "None yet. UI edits and replaced game files show up here.",
            ));
        }
        for (n, f) in others.iter().take(LISTED_FILES).enumerate() {
            let path = f.clone();
            list = list.child(
                div()
                    .id(("other-file", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(12.))
                            .child(f.clone()),
                    )
                    .child(confirm_button(
                        format!("remove-file-{n}"),
                        "Remove",
                        "Really remove?",
                        window,
                        cx,
                        move |this, _, _| {
                            this.project.files.remove(&path);
                            this.finish_project_edit(format!("Removed {path}"));
                        },
                    )),
            );
        }
        if others.len() > LISTED_FILES {
            list = list.child(hint(format!("… and {} more", others.len() - LISTED_FILES)));
        }
        list
    }

    pub fn render_new_project(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let d = self.project_ui.new_project.as_ref()?;
        let id_ok = valid_id(d.id.text.trim());
        let ready = id_ok && !d.name.text.trim().is_empty();
        // Why Create waits, naming the field that is wrong: a set name
        // with a bad id still said "Enter a name".
        let waits = if d.name.text.trim().is_empty() {
            "Enter a name to create the project"
        } else {
            "Fix the Mod id to create the project"
        };
        let shown =
            |edit: &TextEdit<()>, field: NewField| (d.typing == field).then(|| edit.display());
        let field = |id: &'static str, edit: &TextEdit<()>, which: NewField, placeholder| {
            text_field(id, &edit.text, shown(edit, which), placeholder).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(d) = &mut this.project_ui.new_project {
                        d.typing = which;
                    }
                    cx.notify();
                },
            ))
        };
        // Which game it is for, since a project belongs to one.
        let card = widgets::dialog(format!("New {} project", self.game.profile().title.split(':').next().unwrap_or("")))
            .w(px(560.))
            .gap_2()
            .child(hint(
                "A project is one mod: its islands, models and edited files, saved as a mod-loader mod folder.",
            ))
            // Labels above fields, as in the New island dialog.
            // A step down from the description, so the first field does not
            // run into it.
            .child(hint("Name · required").mt_2())
            .child(field("new-project-name", &d.name, NewField::Name, "e.g. Latium Isles"))

            // Until typed into, the id follows the name; the help line says
            // so (in the label it read as a developer's note).
            .child(hint("Mod id"))
            .child(field("new-project-id", &d.id, NewField::Id, "e.g. latium-isles"))
            .child(hint(if id_ok || d.id.text.is_empty() {
                // Why there are two names: the id is what the game and
                // other mods see. Which characters it takes is said only
                // when one is wrong, below.
                format!(
                    "{}{MOD_ID_HELP}",
                    if d.id_typed { "" } else { "Made from the name until you change it. " }
                )
            } else {
                "Not a usable id: letters, digits, '-', '_' and '.', not starting with '.' or '-'".into()
            }))
            .child(hint("Project folder"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        // Its own width, so Change… follows the path it
                        // changes instead of waiting at the dialog's edge.
                        div()
                            .min_w_0()
                            .text_size(px(11.))
                            .flex()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            // The final path, the id part lit as it is
                            // typed; a dim stand-in until there is one.
                            .child(format!(
                                "{}{}",
                                super::short_path(&d.parent),
                                std::path::MAIN_SEPARATOR
                            ))
                            .child(match d.id.text.trim() {
                                // Until there is an id, a token for the slot: a
                                // dim example read as a real destination,
                                // "<mod-id>" as unrendered text, words in
                                // brackets as a folder of that name.
                                // Placeholder text in the slot, unframed: a
                                // framed token read as a field to click.
                                "" => div().text_color(theme::get().disabled).child("‹mod id›"),
                                id => div().text_color(theme::get().accent).child(id.to_string()),
                            }),
                    )
                    // A link after the path: as a full button it weighed as
                    // much as Create and Cancel, for a setting most leave.
                    .child(
                        div()
                            .id("new-project-parent")
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child("Change…")
                            .tooltip(widgets::Tooltip::text(
                                "Choose parent folder",
                                "",
                                "The folder the new mod folder goes in",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                            this.pick_path(
                                "Folder to put the new mod folder in",
                                true,
                                cx,
                                |this, path, _| {
                                    if let Some(d) = &mut this.project_ui.new_project {
                                        d.parent = path;
                                    }
                                },
                                                        );
                        })),
                    ),
            )
            .child(
                // The buttons on a band of their own under a rule, as in
                // the New island dialog.
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .mt_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme::get().border)
                    .items_center()
                    // Why Create waits, in words beside it: a dimmed main
                    // button with no visible reason read as broken.
                    .when(!ready, |d| {
                        d.child(div().flex_1().child(hint(waits)))
                    })
                    .child(action("Cancel", Tone::Secondary).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.project_ui.new_project = None;
                            cx.notify();
                        },
                    )))
                    .child(if ready {
                        action("Create project", Tone::Primary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.create_project(cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    } else {
                        // Not clickable until there is something to create.
                        // The kit's disabled look, not a dimmed primary that
                        // still reads as clickable.
                        widgets::primary_off("Create project", waits)
                            .into_any_element()
                    }),
            );
        Some(widgets::modal("new-project", card))
    }
}

/// The centred column every Project tab sits in, so switching tabs does
/// not change the content width. `page(window).child(page_column(window)…)`.
pub(super) fn page(_window: &Window) -> Div {
    div().w_full().flex().justify_center()
}

pub(super) fn page_column(window: &Window) -> Div {
    let wide = window.viewport_size().width > px(1300.);
    div()
        .w_full()
        .max_w(px(if wide { 1280. } else { 780. }))
        .px_6()
        .py_5()
        .flex()
        .flex_col()
        .gap_4()
}

/// A Project tab's heading, where the Overview has the project's name:
/// the tab's name and one muted line on what it holds. Without it an empty
/// tab was a card floating a third down, which read as a dialog.
pub(super) fn page_header(title: impl Into<SharedString>, about: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(20.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(hint(about))
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
