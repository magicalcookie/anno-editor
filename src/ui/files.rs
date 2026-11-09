// Owns the UI file index and its pane: every compiled UI asset under
// `data/ui/studio/generated/` with what it is and what it is called, so
// screens and templates are found by name instead of by guid. Parsing all
// 4.7k assets takes a few seconds on all cores; the index is cached in the
// config folder and rebuilt when the install's file count changes.
use super::GENERATED;
use crate::editor::{Editor, empty_pane};
use anno_formats::{
    phx::{Asset, Value},
    vfs::Install,
};
use anyhow::Result;
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    icon::Icon,
    theme,
    tree::{self, Tree},
    widgets::Disclosure,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, sync::Arc};

/// Bumped when the cache's shape or contents change.
// 2: sorted by the shown name (title), not the raw root name.
// 3: templates carry a shipped container that shows them (`example`).
// 4: and their root's widget type (`widget_type`).
const VERSION: u32 = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// File name under GENERATED: a guid, or `<name>scene` for screens.
    pub file: String,
    /// PHX kind letters: SC screen, WI widget template, CP component, …
    pub kind: String,
    /// The root object's Name, else the file name.
    pub name: String,
    /// Widgets in the file's own tree (containers' templates not counted).
    pub widgets: u32,
    /// Other assets that reference this one (container templates, styles).
    pub used_by: u32,
    /// A template: a shipped container widget that shows it, as (file,
    /// widget index), one without input overrides when there is one. The
    /// widget library adds a template by copying it.
    #[serde(default)]
    pub example: Option<(String, u32)>,
    /// A template's root WidgetType, or a widget type's parent: the guid
    /// of a widget type asset (Button, or a game type built on one).
    #[serde(default)]
    pub widget_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    version: u32,
    stamp: usize,
    /// Screens first, then templates, then the rest; by name within each.
    pub entries: Vec<Entry>,
    /// Each template's group in the widget library, worked out once the
    /// index loads (`library::group_templates`).
    #[serde(skip)]
    pub groups: Vec<Option<super::library::Group>>,
}

impl Index {
    #[cfg(test)]
    pub(crate) fn of(entries: Vec<Entry>) -> Index {
        let mut i = Index {
            entries,
            ..Default::default()
        };
        i.groups = super::library::group_templates(&i);
        i
    }

    pub fn load_or_build(install: &Install, cache: &Path) -> Result<Index> {
        // The library groups by widget types the Phoenix sources name.
        anno_formats::phx::register_sources(install);
        if let Ok(bytes) = std::fs::read(cache)
            && let Ok(mut i) = serde_json::from_slice::<Index>(&bytes)
            && i.version == VERSION
            && i.stamp == install.file_count()
        {
            i.groups = super::library::group_templates(&i);
            return Ok(i);
        }
        let mut i = Index::build(install);
        if let Some(dir) = cache.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(cache, serde_json::to_vec(&i)?)?;
        i.groups = super::library::group_templates(&i);
        Ok(i)
    }

    pub fn build(install: &Install) -> Index {
        let files: Vec<String> = install
            .list(GENERATED.trim_end_matches('/'))
            .into_iter()
            .map(|p| p.rsplit('/').next().unwrap_or(&p).to_owned())
            .collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = files.len().div_ceil(threads).max(1);
        let scanned: Vec<Scan> = std::thread::scope(|scope| {
            let workers: Vec<_> = files
                .chunks(chunk)
                .map(|files| {
                    scope.spawn(move || {
                        files
                            .iter()
                            .filter_map(|f| scan(install, f))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().expect("UI index worker"))
                .collect()
        });
        let mut used_by: HashMap<String, u32> = HashMap::new();
        // Templates carry no name of their own; the container widgets that
        // expand them do. The most common one names the template.
        let mut called: HashMap<String, HashMap<String, u32>> = HashMap::new();
        // (overridden, file, widget): the first plain one wins.
        let mut example: HashMap<String, (bool, String, u32)> = HashMap::new();
        for s in &scanned {
            for r in &s.refs {
                *used_by.entry(r.clone()).or_default() += 1;
            }
            for c in &s.containers {
                if let Some(name) = &c.name {
                    *called
                        .entry(c.template.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_default() += 1;
                }
                let here = (!c.plain, s.entry.file.clone(), c.widget);
                example
                    .entry(c.template.clone())
                    .and_modify(|e| {
                        if here.0 < e.0 {
                            *e = here.clone();
                        }
                    })
                    .or_insert(here);
            }
        }
        let mut entries: Vec<Entry> = scanned
            .into_iter()
            .map(|s| {
                let mut e = s.entry;
                e.used_by = used_by.get(&e.file).copied().unwrap_or(0);
                e.example = example.get(&e.file).map(|(_, f, w)| (f.clone(), *w));
                if e.name.is_empty() {
                    e.name = called
                        .get(&e.file)
                        .and_then(|c| c.iter().max_by_key(|(n, k)| (**k, std::cmp::Reverse(*n))))
                        .map(|(n, _)| n.clone())
                        .or(s.first)
                        .unwrap_or_else(|| e.file.clone());
                }
                e
            })
            .collect();
        // By the name each row shows: sorted on the raw name, "LS_…"
        // and "…MenuScene" put rows visibly out of order.
        entries.sort_by_cached_key(|e| (kind_rank(&e.kind), title(&e.name).to_lowercase()));
        Index {
            version: VERSION,
            stamp: install.file_count(),
            entries,
            groups: Vec::new(),
        }
    }

    /// Entries whose name, file or kind label contain every word of `query`.
    pub fn find(&self, query: &str, show: Show) -> Vec<usize> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| match show {
                Show::Screens => e.kind == "SC",
                Show::Layouts => e.kind == "SC" || e.kind == "WI",
                Show::All => true,
            })
            .filter(|(_, e)| {
                let hay = format!("{} {} {}", e.name, e.file, kind_label(&e.kind)).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// What one file contributes: its entry, the assets it references, and its
/// container widgets.
struct Scan {
    entry: Entry,
    refs: Vec<String>,
    containers: Vec<Container>,
    /// Name of the first named top-level widget, the last-resort name.
    first: Option<String>,
}

/// A container widget: the template it expands, the name it gives it, its
/// index, and whether it shows the template as shipped (no overrides).
struct Container {
    template: String,
    name: Option<String>,
    widget: u32,
    plain: bool,
}

fn scan(install: &Install, file: &str) -> Option<Scan> {
    let bytes = install.read(&format!("{GENERATED}{file}")).ok()?;
    let asset = Asset::parse(bytes).ok()?;
    let widgets = asset.widgets();
    let root_name = widgets
        .first()
        .and_then(|w| w.name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| {
            let (root, _) = asset.objects().next()?;
            asset
                .object_props(root)
                .into_iter()
                .find_map(|p| match (p.name, p.value) {
                    (Some("Name"), Value::Str(s)) if !s.is_empty() => Some(s),
                    _ => None,
                })
        });
    let containers = widgets
        .iter()
        .enumerate()
        .filter_map(|(i, w)| {
            Some(Container {
                template: asset.container_template(w)?.id.to_string(),
                name: w.name.clone(),
                widget: i as u32,
                plain: super::hierarchy::plain(&asset, w),
            })
        })
        .collect();
    let first = widgets
        .first()
        .and_then(|r| r.children.iter().find_map(|&c| widgets[c].name.clone()));
    Some(Scan {
        entry: Entry {
            file: file.to_owned(),
            kind: asset.header().kind_str(),
            name: root_name.unwrap_or_default(),
            widgets: widgets.len().saturating_sub(1) as u32,
            used_by: 0,
            example: None,
            widget_type: asset.widget_type().map(|r| r.id.to_string()),
        },
        refs: asset
            .cross_refs()
            .assets
            .into_iter()
            .map(|(_, guid, _)| guid.to_string())
            .collect(),
        containers,
        first,
    })
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "SC" => 0,
        "WI" => 1,
        _ => 2,
    }
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "SC" => "screen",
        "WI" => "template",
        "CP" => "component",
        "IM" => "image",
        "TA" => "atlas",
        "TS" => "text style",
        "LF" | "FT" | "FD" | "LG" => "font",
        "PA" => "palette",
        "EN" => "enum",
        "EF" => "effect",
        "VI" => "video",
        "BE" => "widget type",
        "GO" => "game object",
        _ => "other",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Show {
    Screens,
    /// Screens and widget templates: everything with a widget tree.
    #[default]
    Layouts,
    All,
}

/// The pane's search state.
#[derive(Default)]
pub struct Find {
    pub index: Option<Arc<Index>>,
    pub loading: bool,
    query: String,
    editing: Option<TextEdit<()>>,
    show: Show,
    rows: Vec<usize>,
    tree: Tree<usize>,
    /// Groups folded or unfolded against their default (Debug starts
    /// folded, the rest open).
    flipped: std::collections::BTreeSet<u8>,
    /// The widget library's search over the same index.
    pub library: super::library::Library,
}

impl Find {
    /// The search field while it takes keys.
    pub(crate) fn typing(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        if self.editing.is_none() {
            return self.library.typing();
        }
        self.editing
            .as_mut()
            .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer)
    }
}

impl Find {
    /// "3,375 of 4,824 shown · 195 screens, 3,180 templates", for the
    /// status bar; `None` until the index is loaded.
    pub fn shown(&self) -> Option<String> {
        let index = self.index.as_ref()?;
        // Screens and the rest apart: the screen groups hold a few dozen,
        // and a bare 3,375 read as not matching them.
        let screens = (self.rows.iter())
            .filter(|&&r| index.entries[r].kind == "SC")
            .count();
        let rest = self.rows.len() - screens;
        let n = crate::project::count;
        // The rest named by the filter: templates under the default one.
        let rest_name = if self.show == Show::All {
            "other files"
        } else {
            "templates"
        };
        Some(format!(
            "{} of {} shown · {} {} · {} {rest_name}",
            n(self.rows.len()),
            n(index.entries.len()),
            n(screens),
            if screens == 1 { "screen" } else { "screens" },
            n(rest),
        ))
    }
}

/// What a screen is for, guessed from its name, so 3,000 names fall into
/// a handful of groups. Templates and other kinds get their own.
const GROUPS: [&str; 7] = [
    "HUD",
    "Menus",
    "Popups & dialogs",
    "Other screens",
    "Templates",
    "Other files",
    "Debug & tests",
];
const DEBUG: u8 = 6;

fn group_of(e: &Entry) -> u8 {
    let n = e.name.as_str();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if has(&["Debug", "Benchmark", "Console", "Test", "Cheat", "Demo"]) {
        DEBUG
    } else if e.kind == "WI" {
        4
    } else if e.kind != "SC" {
        5
    } else if has(&[
        "Popup",
        "Dialog",
        "Tooltip",
        "Notification",
        "Message",
        "Warning",
    ]) {
        2
    } else if has(&[
        "Menu", "Options", "Settings", "Lobby", "Main", "Credits", "Loading",
    ]) {
        1
    } else if has(&[
        "HUD",
        "Hud",
        "Bar",
        "Overlay",
        "Indicator",
        "Minimap",
        "Cursor",
        "Panel",
    ]) {
        0
    } else {
        3
    }
}

/// `ConstructionMenuScene` → `Construction Menu Scene`: easier to scan;
/// search still matches the stored name.
/// A file's name as its row shows it: every screen is a Scene, so the
/// suffix goes, and words are spaced.
fn title(name: &str) -> String {
    spaced(
        name.strip_suffix("Scene")
            .filter(|n| !n.is_empty())
            .unwrap_or(name),
    )
}

pub(super) fn spaced(name: &str) -> String {
    // "LS_" marks loading-screen files; the group says what they are.
    let name = name.strip_prefix("LS_").unwrap_or(name);
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        // A word starts at a capital after a lower-case letter or digit
        // (`MenuScene`), or at the last capital of an acronym (`HUDArchives`).
        let starts = c.is_ascii_uppercase()
            && prev.is_some_and(|p| {
                p.is_ascii_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
            });
        if starts {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// A row of the file list: a group header or an index entry.
#[derive(Clone, Copy)]
enum FileRow {
    Group(u8, usize),
    Entry(usize),
}

impl Find {
    /// `path`'s screen name as the file list shows it (`Construction Menu
    /// Scene`), once the index is in.
    pub(super) fn display_name(&self, path: &str) -> Option<String> {
        let file = path.strip_prefix(GENERATED)?;
        let index = self.index.as_ref()?;
        let entry = index.entries.iter().find(|e| e.file == file)?;
        // As the list shows it: every screen is a Scene.
        Some(spaced(
            entry
                .name
                .strip_suffix("Scene")
                .filter(|n| !n.is_empty())
                .unwrap_or(&entry.name),
        ))
    }

    /// Every file's name as the list shows it, by guid, once the index is
    /// in: messages name templates the way the list and scope bar do.
    pub(super) fn names(
        &self,
    ) -> Option<std::collections::HashMap<anno_formats::phx::Guid, String>> {
        let index = self.index.as_ref()?;
        Some(
            index
                .entries
                .iter()
                .filter_map(|e| {
                    let g = anno_formats::phx::Guid::parse(&e.file)?;
                    let name = e
                        .name
                        .strip_suffix("Scene")
                        .filter(|n| !n.is_empty())
                        .unwrap_or(&e.name);
                    Some((g, spaced(name)))
                })
                .collect(),
        )
    }

    fn refresh(&mut self) {
        self.rows = self
            .index
            .as_ref()
            .map_or_else(Vec::new, |i| i.find(&self.query, self.show));
        self.library.refresh(self.index.as_deref());
    }

    /// Ends typing into the search fields: a click elsewhere took focus.
    pub fn stop_typing(&mut self) {
        self.editing = None;
        self.library.stop_typing();
    }

    /// Typing into a search field; false when none is focused.
    pub fn key_down(&mut self, event: &KeyDownEvent) -> bool {
        if self.library.key_down(event, self.index.as_deref()) {
            return true;
        }
        let Some(edit) = &mut self.editing else {
            return false;
        };
        match edit.key_down(event) {
            EditEvent::Commit | EditEvent::Cancel => self.editing = None,
            EditEvent::Changed => {
                self.query = edit.text.clone();
                self.refresh();
            }
            // Ctrl 1, Ctrl S and the like stay the editor's shortcuts;
            // plain keys (arrows) must not reach the canvas behind.
            EditEvent::Ignored if event.keystroke.modifiers.control => return false,
            EditEvent::Ignored => {}
        }
        true
    }
}

impl Editor {
    /// Screens worth a one-click open while none is: those this mod has
    /// edited, then a few every modder looks at first. Paths and titles.
    pub(super) fn starter_screens(&self) -> Vec<(String, String)> {
        // TODO(alex): a recently opened list would beat this fixed pick.
        // Not Construction Menu: it is the screen modders want most, but
        // its layout still panics (layout.rs, being fixed elsewhere).
        // Nor Resource Bar or Minimap: their items are runtime lists, and
        // the bar's template is not shipped.
        // Options panics in the same layout code. These two lay out fully.
        const FIRST_LOOKS: [&str; 2] = ["MainTitleScene", "HUDGovernorsBarScene"];
        let Some(index) = &self.ui_find.index else {
            return Vec::new();
        };
        let edited = |e: &&Entry| {
            let path = format!("{GENERATED}{}", e.file);
            (self.project.files).contains_key(&*anno_formats::rda::normalize_path(&path))
        };
        let screens = || index.entries.iter().filter(|e| e.kind == "SC");
        let mut picks: Vec<&Entry> = screens().filter(edited).collect();
        // In FIRST_LOOKS order, not the index's alphabetical one.
        picks.extend(
            FIRST_LOOKS
                .iter()
                .filter_map(|n| screens().find(|e| n.eq_ignore_ascii_case(&e.name) && !edited(e))),
        );
        picks
            .into_iter()
            .take(4)
            .map(|e| {
                (
                    format!("{GENERATED}{}", e.file),
                    format!("Open {}", title(&e.name)),
                )
            })
            .collect()
    }

    /// Builds or loads the UI file index off the UI thread, once. Called
    /// by everything that names screens and templates from it (the UI
    /// files pane, the widget library, opening a screen): from the files
    /// pane alone, a session that opened on the widget tree showed guids
    /// and an empty library (round 15).
    pub(crate) fn ensure_ui_index(&mut self, cx: &mut Context<Self>) {
        let (None, false, Some(install)) = (
            &self.ui_find.index,
            self.ui_find.loading,
            self.install.clone(),
        ) else {
            return;
        };
        self.ui_find.loading = true;
        let cache = self.config.join("ui-index.json");
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move { Index::load_or_build(&install, &cache) })
                .await;
            this.update(cx, |this, cx| {
                this.ui_find.loading = false;
                match built {
                    Ok(i) => {
                        this.ui_find.index = Some(Arc::new(i));
                        this.ui_find.refresh();
                    }
                    Err(e) => this.message(format!("UI index unavailable: {e:#}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn render_ui_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_ui_index(cx);
        let Some(index) = self.ui_find.index.clone() else {
            return empty_pane(if self.install.is_some() {
                "Indexing UI files…"
            } else {
                "Indexing the install…"
            });
        };
        let f = &self.ui_find;
        let query_empty =
            f.query.is_empty() && f.editing.as_ref().is_none_or(|e| e.display().is_empty());
        let search = text_field(
            "ui-files-search",
            &f.query,
            f.editing.as_ref().map(|e| e.display()),
            "Search screens and templates…",
        )
        .flex_1()
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            // One field types at a time; the pane keys go to stays.
            if let Some(ui) = &mut this.ui {
                let keys = ui.keys;
                ui.take_keys(keys);
            }
            this.ui_find.editing =
                Some(TextEdit::new((), this.ui_find.query.clone(), Filter::Text));
            cx.notify();
        }));
        // One choice of three, not two toggles and an add button.
        let filters = div().px_1().child(gpui_editor_kit::widgets::segmented(
            "ui-files-show",
            [
                (Show::Screens, "Screens".into()),
                (Show::Layouts, "Screens + templates".into()),
                (Show::All, "All files".into()),
            ],
            &f.show,
            cx,
            |this, show, _, _| {
                this.ui_find.show = show;
                this.ui_find.refresh();
            },
        ));
        let open = self.ui.as_ref().map(|u| u.path.clone());
        let has_open = open.is_some();
        // Grouped while browsing; a search lists its hits flat.
        let rows: Vec<FileRow> = if f.query.is_empty() {
            let mut by_group: [Vec<usize>; GROUPS.len()] = Default::default();
            for &r in &f.rows {
                by_group[group_of(&index.entries[r]) as usize].push(r);
            }
            let mut rows = Vec::with_capacity(f.rows.len() + GROUPS.len());
            for (g, members) in by_group.iter().enumerate() {
                if members.is_empty() {
                    continue;
                }
                let g = g as u8;
                rows.push(FileRow::Group(g, members.len()));
                if (g != DEBUG) != f.flipped.contains(&g) {
                    rows.extend(members.iter().map(|&r| FileRow::Entry(r)));
                }
            }
            rows
        } else {
            f.rows.iter().map(|&r| FileRow::Entry(r)).collect()
        };
        let grouped = f.query.is_empty();
        let flipped = f.flipped.clone();
        let rows = Arc::new(rows);
        let list_index = index.clone();
        let click_rows = rows.clone();
        let click_index = index.clone();
        let glyph_rows = rows.clone();
        let glyph_index = index.clone();
        let (peek_rows, peek_index) = (rows.clone(), index.clone());
        let count = rows.len();
        let list = tree::List::new(
            "ui-files-rows",
            count,
            move |i| {
                let r = match rows[i] {
                    FileRow::Group(g, n) => {
                        let open = (g != DEBUG) != flipped.contains(&g);
                        return tree::Item {
                            depth: 0,
                            disclosure: if open {
                                Disclosure::Expanded
                            } else {
                                Disclosure::Collapsed
                            },
                            // Counted in the right column, as the Islands
                            // and Hierarchy groups are.
                            label: GROUPS[g as usize].into(),
                            detail: crate::project::count(n).into(),
                            selected: false,
                        };
                    }
                    FileRow::Entry(r) => r,
                };
                let e = &list_index.entries[r];
                // The kind is the row's glyph. No per-file number: a file's
                // own widget count (mostly 1 or 2; templates bring the rest)
                // read as unexplained noise, headed or bare. The Widget tree
                // header counts what a screen shows once it is open.
                tree::Item {
                    depth: usize::from(grouped),
                    disclosure: Disclosure::Leaf,
                    // Every screen is a Scene, and under "Popups & dialogs"
                    // a trailing Popup; the group says which kind.
                    label: {
                        let name = title(&e.name);
                        // The group already says these are popups.
                        if grouped && group_of(e) == 2 {
                            name.strip_suffix(" Popup")
                                .filter(|n| !n.is_empty())
                                .map_or(name.clone(), str::to_owned)
                        } else {
                            name
                        }
                        .into()
                    },
                    detail: SharedString::default(),
                    selected: open.as_deref() == Some(&format!("{GENERATED}{}", e.file)),
                }
            },
            move |this: &mut Editor, i, event, cx| {
                let r = match click_rows[i] {
                    FileRow::Group(g, _) => {
                        let flipped = &mut this.ui_find.flipped;
                        if !flipped.remove(&g) {
                            flipped.insert(g);
                        }
                        return;
                    }
                    FileRow::Entry(r) => r,
                };
                let e = &click_index.entries[r];
                let path = format!("{GENERATED}{}", e.file);
                // Shift adds the screen to the open file's game view.
                if event.modifiers().shift
                    && let Some(ui) = &mut this.ui
                {
                    match ui.toggle_screen(&path) {
                        Ok(m) => this.message(m, false),
                        Err(e) => this.message(format!("{e:#}"), true),
                    }
                    return;
                }
                this.open_ui_asset(&path, cx);
            },
        )
        .glyph(move |i| match glyph_rows[i] {
            FileRow::Group(..) => None,
            FileRow::Entry(r) => Some(match glyph_index.entries[r].kind.as_str() {
                "SC" => Icon::LAYOUT,
                "WI" => Icon::CUBE,
                _ => Icon::FILE,
            }),
        })
        // A hovered file is described in the empty Inspector, as a hovered
        // island is: file facts only, so it holds while screens cannot lay
        // out.
        .decorate({
            let entity = cx.entity().downgrade();
            move |i, row| {
                let FileRow::Entry(r) = peek_rows[i] else {
                    return row;
                };
                let (entry, entity) = (peek_index.entries[r].clone(), entity.clone());
                row.on_hover(move |hovered, _, cx| {
                    let entry = entry.clone();
                    let _ = entity.update(cx, |this, cx| {
                        this.after_dwell(entry.file.clone(), *hovered, cx, move |this, cx| {
                            if this.ui.is_none() && this.ui_peek.as_ref() != Some(&entry) {
                                let first = this.ui_peek.is_none();
                                this.ui_peek = Some(entry);
                                this.notify_pane(crate::editor::Pane::UiInspector, cx);
                                // The canvas's starters step down once a
                                // preview shows.
                                if first {
                                    this.notify_pane(crate::editor::Pane::UiCanvas, cx);
                                }
                            }
                        })
                    });
                })
            }
        })
        .render(&f.tree, window, cx)
        .flex_1()
        .min_h_0();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(
                div()
                    .relative()
                    .flex()
                    .gap_1()
                    .p_1()
                    .child(search)
                    // A kept query reads as the screen's title without a way
                    // out of it, so a filled search gets a clear button.
                    .when(!query_empty, |d| {
                        d.child(
                            div()
                                .id("ui-files-clear")
                                .absolute()
                                .right(px(10.))
                                .top(px(4.))
                                .bottom(px(4.))
                                .flex()
                                .items_center()
                                .px_1()
                                .cursor_pointer()
                                .text_color(theme::get().muted)
                                .hover(|s| s.text_color(theme::get().text))
                                .child("×")
                                .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                                    "Clear search",
                                    "",
                                    "Show every file again, grouped",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.ui_find.query.clear();
                                    this.ui_find.editing = None;
                                    this.ui_find.refresh();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(filters)
            .child(list)
            .child(
                // Short enough for a narrow pane; the rest on hover.
                div()
                    .id("ui-files-footer")
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    // The count went to the status bar, where Assets and
                    // Island keep theirs; the footer keeps the one hidden
                    // gesture.
                    // Nothing open: the canvas already says to pick a screen.
                    .when(has_open, |d| {
                        d.child("Shift-click a screen to stack it on the open one")
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, Index, Install, Show, VERSION, spaced};

    #[test]
    fn names_split_into_words_around_acronyms() {
        assert_eq!(spaced("ConstructionMenuScene"), "Construction Menu Scene");
        assert_eq!(spaced("HUDArchivesScene"), "HUD Archives Scene");
        assert_eq!(spaced("MinimapA7Scene"), "Minimap A7 Scene");
        assert_eq!(spaced("DLC"), "DLC");
        assert_eq!(spaced("LS_CharacterNotification"), "Character Notification");
    }

    fn entry(file: &str, kind: &str, name: &str) -> Entry {
        Entry {
            file: file.into(),
            kind: kind.into(),
            name: name.into(),
            widgets: 1,
            used_by: 0,
            example: None,
            widget_type: None,
        }
    }

    #[test]
    fn find_matches_every_word_and_filters_kinds() {
        let index = Index {
            version: VERSION,
            stamp: 0,
            entries: vec![
                entry("minimapscene", "SC", "MinimapScene"),
                entry("005e6a6d", "WI", "QuestTracker"),
                entry("0143fdc2", "CP", "StackLayout"),
            ],
            groups: Vec::new(),
        };
        assert_eq!(index.find("map", Show::Layouts), [0]);
        assert_eq!(index.find("QUEST tracker", Show::Layouts), [1]);
        assert_eq!(index.find("", Show::Screens), [0]);
        assert_eq!(index.find("", Show::Layouts), [0, 1]);
        assert_eq!(index.find("component", Show::All), [2]);
    }

    // Needs the game: screens are found by name and come first.
    #[test]
    fn install_index_names_screens() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let index = Index::build(&install);
        assert!(index.entries.len() > 4000);
        assert_eq!(index.entries[0].kind, "SC");
        let hits = index.find("minimap", Show::Screens);
        assert!(
            hits.iter()
                .any(|&i| index.entries[i].file == "minimapscene"),
            "{:?}",
            hits.iter()
                .map(|&i| &index.entries[i].name)
                .collect::<Vec<_>>()
        );
        let quests = &index.entries[index.find("005e6a6d", Show::All)[0]];
        assert_eq!(quests.kind, "WI");
        assert_ne!(quests.name, quests.file, "templates carry a name");
    }
}

impl Editor {
    /// The described file, only beside the list it came from: on the
    /// Widget tree tab it read as left over, and the canvas and Inspector
    /// disagreed when one checked and the other did not.
    pub(super) fn ui_peek_shown(&self) -> Option<&Entry> {
        self.ui_peek.as_ref().filter(|_| {
            self.dock_visible_panes()
                .contains(&crate::editor::Pane::UiFiles)
        })
    }

    /// The hovered UI file, in the Inspector while no screen is open.
    pub(super) fn ui_peek_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.ui_peek_shown()?;
        let kind = match e.kind.as_str() {
            "SC" => "Screen",
            "WI" => "Widget template",
            _ => "UI file",
        };
        let mut facts = vec![kind.to_owned(), GROUPS[group_of(e) as usize].to_owned()];
        // Its own tree's widgets, "in this file": a bare "2 widgets" read
        // as the whole screen, and "own widgets" was the code's word.
        // Templates it expands bring more.
        facts.push(format!(
            "{} {} in this file",
            crate::project::count(e.widgets as usize),
            if e.widgets == 1 { "widget" } else { "widgets" }
        ));
        if e.used_by > 0 {
            facts.push(format!(
                "used by {}",
                crate::project::count(e.used_by as usize)
            ));
        }
        let path = format!("{GENERATED}{}", e.file);
        let name = title(&e.name);
        Some(
            div()
                .p_2()
                // The pane's width, so a long path cuts with an ellipsis
                // instead of running past the edge.
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap_1()
                // "Details", not "preview": with no picture until screens lay
                // out, a "preview" caption promised what the pane lacked.
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(match e.kind.as_str() {
                            // "Preview", as the Island workspace's: what the
                            // pointer is over, not an opened file.
                            "SC" => "Screen preview",
                            "WI" => "Template preview",
                            _ => "File preview",
                        }),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(facts.join(" · ")),
                )
                // The path on a line of its own, whole on hover; copying it
                // under it. On one line a long path ran into Copy path.
                .child(
                    div()
                        .id("ui-peek-path")
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                            "Path",
                            "",
                            path.clone(),
                        ))
                        .child(path.clone()),
                )
                .child(div().flex().text_size(px(11.)).child({
                    let copied = path.clone();
                    div()
                        .id("ui-peek-copy-path")
                        .flex_shrink_0()
                        .text_color(theme::get().accent)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child("Copy path")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                            this.message("Copied the path", false);
                            cx.notify();
                        }))
                }))
                // Open, then the file among its neighbours in Assets, as the
                // Island preview offers.
                .child({
                    let shown = path.clone();
                    div()
                        .mt_1()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_3()
                        .child(
                            gpui_editor_kit::widgets::action(
                                // Secondary: a preview of what the pointer is
                                // over, not a choice; the canvas keeps the one
                                // filled button.
                                format!("Open {name}"),
                                gpui_editor_kit::widgets::Tone::Secondary,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.open_ui_asset(&path, cx);
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            div()
                                .id("ui-peek-show-in-assets")
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
}
