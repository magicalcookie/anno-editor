// The Map workspace's side panes and dialogs: the slot list, the inspector
// (a slot's role, class, island, turns and corner, or the map's facts), the
// island picker that drags islands onto the map, and the New map, Open
// shipped map and save-a-shipped-map dialogs.

use super::{MapEditor, SaveAs, layout_clash, map_files, read_layout, view::variant_label};
use crate::editor::{Editor, Leave, Saved};
use anno_core::island::{Level, Names};
use anno_core::map::{Registers, Role, ShippedMap, Slot};
use gpui_editor_kit::{
    field::{EditEvent, Filter, NumberAction, TextEdit, number_field, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{self, button, dialog, hint, info_row, section},
};
use std::sync::Arc;

/// An island being dragged from the Islands tab, by path.
#[derive(Clone)]
pub struct IslandDrag(pub String);

/// What a new layout starts from.
#[derive(Clone, Debug, PartialEq)]
pub enum From {
    /// No slots on the base's sea (`Maps::new_map`).
    Empty,
    /// A copy of a shipped or project layout, by path.
    Copy(String),
}

/// Which list the New map dialog shows under Start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Empty,
    Shipped,
    Project,
}

pub struct NewMap {
    pub name: TextEdit<()>,
    pub region: String,
    /// The shipped map whose sea an empty layout takes.
    pub base: String,
    pub source: Source,
    pub from: From,
    /// The map files a layout from the chosen start writes, by the path it
    /// starts from (`probe`); worked out off the UI thread on each choice.
    pub probe: Option<(String, Vec<String>)>,
}

/// Ctrl S on a shipped map: save it as a new layout under `name`, or
/// override it.
pub struct SaveShipped {
    pub name: TextEdit<()>,
    /// The map's files as a new layout writes them (`probe`).
    pub probe: Vec<String>,
    /// Where to go once the map is written: Save and quit, Save and open.
    /// Closing the dialog drops it with the save.
    pub then: Option<Leave>,
}

/// Map → Open map…: the project's layouts, then the shipped maps by
/// region, narrowed by `filter`; `at` is the row Enter opens, counting
/// only the maps the filter keeps.
pub struct OpenMap {
    pub filter: TextEdit<()>,
    pub at: usize,
}

/// The Map workspace's modal, while one is open.
pub enum Dialog {
    New(NewMap),
    Open(OpenMap),
    SaveShipped(SaveShipped),
}

/// A row of the Open dialog: a group heading, or a map to open.
pub(crate) enum OpenRow {
    Heading(String),
    Map {
        path: String,
        name: String,
        detail: String,
        open: bool,
    },
}

/// The Open dialog's rows, one height each, so a row's place in the list
/// is its index times this.
const OPEN_ROW: f32 = 28.;

/// What `probe` puts where a layout's file stem goes.
const STEM: &str = "{stem}";

/// A layout's map files as the game writes them (`Maps::write`), with
/// `STEM` for the stem: "Saves to" and the name check fill in the stem as
/// the name is typed. Empty when the map doesn't write.
pub(crate) fn probe(
    maps: &dyn anno_core::map::Maps,
    install: &anno_formats::vfs::Install,
    map: &anno_core::map::Map,
) -> Vec<String> {
    (maps.write(install, map, STEM)).map_or_else(|_| Vec::new(), |files| map_files(&files))
}

/// `probe`'s paths for a layout named `name`.
fn with_stem(probe: &[String], name: &str) -> Vec<String> {
    let stem = crate::island::file_name(name);
    probe.iter().map(|p| p.replace(STEM, &stem)).collect()
}

/// The folders `paths` are in, each once, in order.
fn folders(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let folder = p.rsplit_once('/').map_or("", |(f, _)| f);
        let folder = format!("{folder}/");
        if !out.contains(&folder) {
            out.push(folder);
        }
    }
    out
}

/// "Saves to" with the folders of `paths`, one a line.
fn saves_to(paths: &[String]) -> Div {
    let folders = folders(paths);
    let head = match folders.len() {
        1 => "Saves to",
        _ => "Saves to, one folder a variant:",
    };
    div()
        .flex()
        .flex_col()
        .child(hint(head))
        .children(folders.into_iter().map(hint))
}

impl Editor {
    /// Map → New map layout…, asking first when the open map has edits
    /// (`Editor::leave`).
    pub fn open_new_map(&mut self, cx: &mut Context<Self>) {
        self.leave(Leave::NewMap, cx);
    }

    /// Opens the New map layout dialog. The open map stays until Create,
    /// so Cancel keeps it.
    pub(crate) fn new_map_dialog(&mut self, cx: &mut Context<Self>) {
        if self.maps().is_none() {
            return self.message("This game's maps don't open in the editor", true);
        }
        let region = self.island_rules().default_region.to_ascii_lowercase();
        self.map_dialog = Some(Dialog::New(NewMap {
            name: TextEdit::new((), "", Filter::Text),
            region,
            base: String::new(),
            source: Source::Empty,
            from: From::Empty,
            probe: None,
        }));
        self.load_shipped_maps(cx);
        self.pick_default_base(cx);
    }

    /// The question Ctrl S on an edited shipped map asks, its name field
    /// holding a name no layout of the project has.
    pub(crate) fn save_shipped_dialog(&self) -> SaveShipped {
        let probe = match (self.maps(), self.install.as_deref(), &self.map) {
            (Some(maps), Some(install), Some(m)) => probe(maps, install, &m.map),
            _ => Vec::new(),
        };
        let shipped = self.map.as_ref().map_or_else(String::new, |m| m.title());
        // Its own name, so its files don't sit under the shipped one's in
        // the project's custom folder: "custom", then a number.
        let base = match shipped.to_ascii_lowercase().contains("custom") {
            true => shipped,
            false => format!("{shipped} custom"),
        };
        let free =
            |n: &str| layout_clash(&self.project.layouts, n, &with_stem(&probe, n), None).is_none();
        let name = std::iter::once(base.clone())
            .chain((2..100).map(|n| format!("{base} {n}")))
            .find(|n| free(n))
            .unwrap_or(base);
        SaveShipped {
            name: TextEdit::new((), name, Filter::Text),
            probe,
            then: None,
        }
    }

    /// Works out where a layout from the New map dialog's start saves,
    /// once per start, off the UI thread: the game reads the start's files
    /// to write it.
    fn probe_new_map(&mut self, cx: &mut Context<Self>) {
        let (Some(maps), Some(install)) = (self.maps(), self.install.clone()) else {
            return;
        };
        let Some(Dialog::New(d)) = &self.map_dialog else {
            return;
        };
        let key = match &d.from {
            From::Empty => d.base.clone(),
            From::Copy(path) => path.clone(),
        };
        if key.is_empty() || d.probe.as_ref().is_some_and(|p| p.0 == key) {
            return;
        }
        let empty = d.from == From::Empty;
        let entry = (self.project.layouts.iter())
            .find(|l| l.path == key)
            .cloned();
        cx.spawn(async move |this, cx| {
            let paths = cx
                .background_spawn({
                    let key = key.clone();
                    async move {
                        let map = match (empty, entry) {
                            (true, _) => maps.new_map(&install, &key, "probe"),
                            (false, Some(entry)) => read_layout(maps, &install, &entry),
                            (false, None) => maps.read(&install, &key),
                        };
                        map.map_or_else(|_| Vec::new(), |m| probe(maps, &install, &m))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(Dialog::New(d)) = &mut this.map_dialog {
                    let now = match &d.from {
                        From::Empty => &d.base,
                        From::Copy(path) => path,
                    };
                    if *now == key {
                        d.probe = Some((key, paths));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Map → Open map…: picking a row asks about the open map's edits;
    /// Cancel keeps it as it is.
    pub fn open_map_dialog(&mut self, cx: &mut Context<Self>) {
        if self.maps().is_none() {
            return self.message("This game's maps don't open in the editor", true);
        }
        self.map_dialog = Some(Dialog::Open(OpenMap {
            filter: TextEdit::new((), "", Filter::Text),
            at: 0,
        }));
        self.scrolls
            .get("map-open-list")
            .set_offset(point(px(0.), px(0.)));
        self.load_shipped_maps(cx);
    }

    /// The Open dialog's rows for `filter`: every project layout, the open
    /// one marked, then the shipped maps under their region and group. A
    /// map stays when each word of the filter is in its name, details or
    /// path.
    pub(crate) fn open_rows(&self, filter: &str) -> Vec<OpenRow> {
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        let keeps = |text: &str| {
            let text = text.to_lowercase();
            words.iter().all(|w| text.contains(w.as_str()))
        };
        let open = self.map.as_ref().and_then(|m| m.path.as_deref());
        let rules = self.island_rules();
        let mut rows = Vec::new();
        let ours: Vec<OpenRow> = (self.project.layouts.iter())
            .map(|l| {
                let detail = layout_detail(rules, l);
                (l, detail)
            })
            .filter(|(l, detail)| keeps(&format!("{} {detail} {}", l.name, l.path)))
            .map(|(l, detail)| OpenRow::Map {
                path: l.path.clone(),
                name: l.name.clone(),
                detail,
                open: open == Some(l.path.as_str()),
            })
            .collect();
        if !ours.is_empty() {
            rows.push(OpenRow::Heading("This project's layouts".into()));
            rows.extend(ours);
        }
        let list = self.shipped_maps.as_deref().map_or(&[][..], Vec::as_slice);
        let worlds = worlds_differ(self.maps().map(|m| m.rules()));
        let mut heading = String::new();
        for s in list {
            let group = match s.region.is_empty() {
                true => pretty_words(&s.group),
                false => format!("{} · {}", rules.province(&s.region), pretty_words(&s.group)),
            };
            let name = shipped_name(s);
            let detail = shipped_detail(s, worlds);
            if !keeps(&format!("{name} {detail} {group} {}", s.path)) {
                continue;
            }
            if group != heading {
                rows.push(OpenRow::Heading(group.clone()));
                heading = group;
            }
            let open = self
                .map
                .as_ref()
                .is_some_and(|m| m.shipped && m.is_at(&s.path));
            rows.push(OpenRow::Map {
                path: s.path.clone(),
                name,
                detail,
                open,
            });
        }
        rows
    }

    /// Opens the Open dialog's row `at` (among the maps its filter keeps).
    fn open_dialog_row(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(Dialog::Open(d)) = &self.map_dialog else {
            return;
        };
        let path = (self.open_rows(&d.filter.text).into_iter())
            .filter_map(|r| match r {
                OpenRow::Map { path, .. } => Some(path),
                OpenRow::Heading(_) => None,
            })
            .nth(at);
        if let Some(path) = path {
            self.map_dialog = None;
            self.open_map(&path, cx);
        }
    }

    /// Up and Down in the Open dialog: the next map row, scrolled into
    /// view.
    fn open_dialog_step(&mut self, by: isize) {
        let Some(Dialog::Open(d)) = &self.map_dialog else {
            return;
        };
        let rows = self.open_rows(&d.filter.text);
        let maps: Vec<usize> = (rows.iter().enumerate())
            .filter(|(_, r)| matches!(r, OpenRow::Map { .. }))
            .map(|(i, _)| i)
            .collect();
        if maps.is_empty() {
            return;
        }
        let at = (d.at as isize + by).clamp(0, maps.len() as isize - 1) as usize;
        let handle = self.scrolls.get("map-open-list");
        let view = f32::from(handle.bounds().size.height);
        let top = maps[at] as f32 * OPEN_ROW;
        let scrolled = -f32::from(handle.offset().y);
        // The heading above the first row of a group comes into view too.
        let want = if top - OPEN_ROW < scrolled {
            (top - OPEN_ROW).max(0.)
        } else if top + OPEN_ROW > scrolled + view {
            top + OPEN_ROW - view
        } else {
            scrolled
        };
        handle.set_offset(point(px(0.), px(-want)));
        if let Some(Dialog::Open(d)) = &mut self.map_dialog {
            d.at = at;
        }
    }

    /// The shipped layouts, listed off the UI thread once.
    pub(crate) fn load_shipped_maps(&mut self, cx: &mut Context<Self>) {
        let (Some(maps), Some(install)) = (self.maps(), self.install.clone()) else {
            return;
        };
        if self.shipped_maps.is_some() || self.shipped_maps_loading {
            return;
        }
        // 117 and 1800 list their maps by asset: wait for the database, or
        // the list comes back without them. The map view asks again.
        if self.assets.is_none() && self.module.database().is_some() {
            return;
        }
        self.shipped_maps_loading = true;
        let assets = self.assets.clone();
        cx.spawn(async move |this, cx| {
            let list = cx
                .background_spawn(async move { maps.shipped(&install, assets.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                this.shipped_maps = Some(Arc::new(list));
                this.shipped_maps_loading = false;
                this.pick_default_base(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The New map dialog's base: scenario 06 for 2070, the only world
    /// checked in game; else the region's first base map.
    fn pick_default_base(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::New(d)) = &mut self.map_dialog else {
            return;
        };
        if !d.base.is_empty() {
            return self.probe_new_map(cx);
        }
        let Some(list) = self.shipped_maps.as_deref() else {
            return;
        };
        let region = d.region.to_ascii_lowercase();
        let bases: Vec<&ShippedMap> = (list.iter())
            .filter(|s| s.base && (s.region.is_empty() || s.region.eq_ignore_ascii_case(&region)))
            .collect();
        let pick = (bases.iter())
            .find(|s| s.path.ends_with("/06.www"))
            .or_else(|| bases.iter().find(|s| s.name.contains("default")))
            .or(bases.first());
        if let Some(s) = pick {
            d.base = s.path.clone();
        }
        self.probe_new_map(cx);
    }

    /// Keys while a map dialog is open.
    pub(crate) fn map_dialog_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.map_dialog = None,
            "enter" if matches!(self.map_dialog, Some(Dialog::New(_))) => self.create_map(cx),
            "enter" if matches!(self.map_dialog, Some(Dialog::SaveShipped(_))) => {
                self.save_shipped_as(SaveAs::NewLayout, cx)
            }
            "enter" if matches!(self.map_dialog, Some(Dialog::Open(_))) => {
                let at = match &self.map_dialog {
                    Some(Dialog::Open(d)) => d.at,
                    _ => 0,
                };
                self.open_dialog_row(at, cx);
            }
            "up" if matches!(self.map_dialog, Some(Dialog::Open(_))) => self.open_dialog_step(-1),
            "down" if matches!(self.map_dialog, Some(Dialog::Open(_))) => self.open_dialog_step(1),
            _ => {
                let field = match &mut self.map_dialog {
                    Some(Dialog::New(d)) => &mut d.name,
                    Some(Dialog::SaveShipped(d)) => &mut d.name,
                    Some(Dialog::Open(d)) => &mut d.filter,
                    None => return,
                };
                let before = field.text.clone();
                if field.key_down_in(event, cx) == EditEvent::Ignored {
                    return;
                }
                // A new filter starts at its first map, scrolled to the top.
                if let Some(Dialog::Open(d)) = &mut self.map_dialog
                    && d.filter.text != before
                {
                    d.at = 0;
                    self.scrolls
                        .get("map-open-list")
                        .set_offset(point(px(0.), px(0.)));
                }
            }
        }
        cx.notify();
    }

    pub fn render_map_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let card = match self.map_dialog.as_ref()? {
            Dialog::New(d) => self.new_map_card(d, window, cx),
            Dialog::Open(d) => self.open_map_card(d, cx),
            Dialog::SaveShipped(d) => self.save_shipped_card(d, cx),
        };
        Some(
            widgets::modal(
                "map-dialog",
                card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.map_dialog = None;
                    cx.notify();
                }),
            ),
        )
    }

    fn new_map_card(&self, d: &NewMap, _: &mut Window, cx: &mut Context<Self>) -> Div {
        let worlds = worlds_differ(self.maps().map(|m| m.rules()));
        let list = self.shipped_maps.as_deref().map_or(&[][..], Vec::as_slice);
        let mut regions: Vec<String> = (list.iter())
            .filter(|s| s.base && !s.region.is_empty())
            .map(|s| s.region.to_ascii_lowercase())
            .collect();
        regions.sort();
        regions.dedup();
        let region = d.region.to_ascii_lowercase();
        let in_region =
            |s: &&ShippedMap| s.region.is_empty() || s.region.eq_ignore_ascii_case(&region);
        let rows: Vec<(String, String, bool)> = match d.source {
            Source::Empty => (list.iter())
                .filter(|s| s.base)
                .filter(in_region)
                .map(|s| (s.path.clone(), shipped_row(s, worlds), s.path == d.base))
                .collect(),
            Source::Shipped => (list.iter())
                .filter(in_region)
                .map(|s| {
                    (
                        s.path.clone(),
                        shipped_row(s, worlds),
                        d.from == From::Copy(s.path.clone()),
                    )
                })
                .collect(),
            Source::Project => (self.project.layouts.iter())
                .map(|l| {
                    (
                        l.path.clone(),
                        l.name.clone(),
                        d.from == From::Copy(l.path.clone()),
                    )
                })
                .collect(),
        };
        let source = d.source;
        let rows = div()
            .flex()
            .flex_col()
            .gap_px()
            .children(rows.into_iter().map(|(path, label, on)| {
                button(label, on)
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Dialog::New(d)) = &mut this.map_dialog {
                            match source {
                                Source::Empty => d.base = path.clone(),
                                _ => d.from = From::Copy(path.clone()),
                            }
                        }
                        this.probe_new_map(cx);
                        cx.notify();
                    }))
            }));
        let empty_note = match (self.shipped_maps.is_none(), d.source) {
            (true, _) => Some("Listing the shipped maps…"),
            (_, Source::Project) if self.project.layouts.is_empty() => {
                Some("This project has no layouts yet")
            }
            _ => None,
        };
        let named = !d.name.text.trim().is_empty();
        let probe = (d.probe.as_ref()).filter(|p| match &d.from {
            From::Empty => p.0 == d.base,
            From::Copy(path) => p.0 == *path,
        });
        let paths = probe.map(|p| with_stem(&p.1, &d.name.text));
        let clash = named
            .then(|| {
                let paths = paths.as_deref().unwrap_or(&[]);
                layout_clash(&self.project.layouts, &d.name.text, paths, None)
            })
            .flatten();
        let ready = named
            && clash.is_none()
            && match (&d.source, &d.from) {
                (Source::Empty, _) => !d.base.is_empty(),
                (_, From::Copy(path)) => !path.is_empty(),
                _ => false,
            };
        dialog("New map layout")
            .w(px(640.))
            .gap_2()
            .child(hint("Name · required"))
            .child(text_field(
                "map-name",
                &d.name.text,
                Some(d.name.display()),
                "e.g. Twin Straits",
            ))
            .when(regions.len() > 1, |c| {
                c.child(hint("Region")).child(widgets::segmented(
                    "map-region",
                    regions
                        .iter()
                        .map(|r| (r.clone(), self.island_rules().province(r).into())),
                    &region,
                    cx,
                    |this, r, _, cx| {
                        if let Some(Dialog::New(d)) = &mut this.map_dialog {
                            d.region = r;
                            d.base.clear();
                            d.from = From::Empty;
                        }
                        this.pick_default_base(cx);
                    },
                ))
            })
            .child(hint("Start from"))
            .child(widgets::segmented(
                "map-source",
                [
                    (Source::Empty, "Empty sea".into()),
                    (Source::Shipped, "Copy of a shipped layout".into()),
                    (Source::Project, "Copy of a project layout".into()),
                ],
                &d.source,
                cx,
                |this, s, _, cx| {
                    if let Some(Dialog::New(d)) = &mut this.map_dialog {
                        d.source = s;
                        if s == Source::Empty {
                            d.from = From::Empty;
                        } else if matches!(d.from, From::Empty) {
                            d.from = From::Copy(String::new());
                        }
                    }
                    this.probe_new_map(cx);
                },
            ))
            .child(hint(match d.source {
                Source::Empty => {
                    "Base sea: the map's size, sea and playable area come from it; no islands"
                }
                Source::Shipped => {
                    "Every slot, island and start of the chosen map, under your name"
                }
                Source::Project => "A copy of one of this project's layouts",
            }))
            .child(
                widgets::vscroll("map-new-list", &self.scrolls.get("map-new-list"), rows)
                    .h(px(260.))
                    .border_1()
                    .border_color(theme::get().border),
            )
            .children(empty_note.map(hint))
            .when_some(clash.clone(), |c, why| {
                c.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::get().error)
                        .child(why),
                )
            })
            .when(named && clash.is_none(), |c| match &paths {
                Some(paths) if !paths.is_empty() => c.child(saves_to(paths)),
                _ => c,
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                            this.map_dialog = None;
                            cx.notify();
                        })),
                    )
                    .child(if ready {
                        widgets::action("Create", widgets::Tone::Primary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.create_map(cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    } else {
                        widgets::primary_off(
                            "Create",
                            if !named {
                                "Name it first"
                            } else if clash.is_some() {
                                "Give it a name no layout of the project has"
                            } else {
                                "Choose what it starts from"
                            },
                        )
                        .into_any_element()
                    }),
            )
    }

    fn open_map_card(&self, d: &OpenMap, cx: &mut Context<Self>) -> Div {
        let rows = self.open_rows(&d.filter.text);
        let mut body = div().flex().flex_col();
        let mut n = 0;
        let found = rows.iter().any(|r| matches!(r, OpenRow::Map { .. }));
        for row in rows {
            match row {
                OpenRow::Heading(text) => {
                    body = body.child(
                        div()
                            .h(px(OPEN_ROW))
                            .flex()
                            .items_end()
                            .px_2()
                            .pb_1()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::get().muted)
                            .child(text),
                    )
                }
                OpenRow::Map {
                    path,
                    name,
                    detail,
                    open,
                } => {
                    let at = n;
                    let current = at == d.at;
                    body = body.child(
                        div()
                            .id(("map-open-row", at))
                            .h(px(OPEN_ROW))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .rounded(px(3.))
                            .cursor_pointer()
                            .when(current, |r| r.bg(theme::get().selected))
                            .when(!current, |r| r.hover(|r| r.bg(theme::get().hover)))
                            .child(div().flex_shrink_0().text_size(px(13.)).child(name))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme::get().muted)
                                    .child(detail),
                            )
                            .when(open, |r| {
                                r.child(widgets::badge("open", theme::get().accent))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_dialog_row(at, cx);
                                cx.notify();
                            }))
                            .tooltip(widgets::Tooltip::text(path.clone(), "", "")),
                    );
                    n += 1;
                }
            }
        }
        let overrides = self
            .maps()
            .is_some_and(|m| m.rules().registers != Registers::None);
        let about = match overrides {
            true => {
                "A shipped map opens with every variant. Saving one asks whether to keep it as a project layout or to override the shipped map."
            }
            false => {
                "A shipped map opens with every variant. Saving one keeps it as a project layout."
            }
        };
        dialog("Open map")
            .w(px(640.))
            .gap_2()
            .child(text_field(
                "map-open-filter",
                &d.filter.text,
                Some(d.filter.display()),
                "Filter by name, region or size",
            ))
            .child(
                widgets::vscroll("map-open-list", &self.scrolls.get("map-open-list"), body)
                    .h(px(420.))
                    .border_1()
                    .border_color(theme::get().border),
            )
            .when(self.shipped_maps.is_none(), |c| {
                c.child(hint("Listing the shipped maps…"))
            })
            .when(self.shipped_maps.is_some() && !found, |c| {
                c.child(hint("No map matches the filter"))
            })
            .child(hint(about))
            .child(hint("Up and Down pick a map, Enter opens it"))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .child(
                        button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                            this.map_dialog = None;
                            cx.notify();
                        })),
                    ),
            )
    }

    fn save_shipped_card(&self, d: &SaveShipped, cx: &mut Context<Self>) -> Div {
        let registers = self.maps().map(|m| m.rules().registers);
        let overrides = registers.is_some_and(|r| r != Registers::None);
        // What the game does with the copy: 1800 can't list a map of its
        // own, so a layout takes the place of the shipped maps it plays as.
        let new_layout = match registers {
            Some(Registers::NewMapType) => {
                "Save as new layout: a copy in the project under the name below, listed in setup as a map of its own."
            }
            Some(Registers::ReplacesShipped) => {
                "Save as new layout: a copy in the project under the name below, loaded wherever the shipped maps it plays as would be (Inspector → Plays as)."
            }
            _ => {
                "Save as new layout: a copy in the project under the name below, which Make test map can build on."
            }
        };
        let shipped = self.map.as_ref().map_or_else(String::new, |m| m.title());
        let name = d.name.text.trim();
        let paths = with_stem(&d.probe, name);
        let clash = match name.is_empty() {
            true => Some("Name the layout first".to_owned()),
            false => layout_clash(&self.project.layouts, name, &paths, None),
        };
        let save = match &clash {
            None => widgets::action("Save as new layout", widgets::Tone::Primary)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save_shipped_as(SaveAs::NewLayout, cx);
                    cx.notify();
                }))
                .into_any_element(),
            Some(why) => widgets::primary_off("Save as new layout", why.clone()).into_any_element(),
        };
        dialog("Save a shipped map")
            .w(px(560.))
            .gap_2()
            .child(format!("{shipped} is one of the game's own maps."))
            .child(hint(new_layout))
            .child(hint("Layout name"))
            .child(text_field(
                "map-save-name",
                &d.name.text,
                Some(d.name.display()),
                "e.g. Twin Straits",
            ))
            .when_some(clash.filter(|_| !name.is_empty()), |c, why| {
                c.child(div().text_size(px(12.)).text_color(theme::get().error).child(why))
            })
            .when(!paths.is_empty(), |c| c.child(saves_to(&paths)))
            .when(overrides, |c| {
                c.child(hint("Override in place: the project replaces the shipped map wherever the game uses it."))
            })
            .when_some(d.then.as_ref(), |c, then| {
                c.child(hint(format!(
                    "Once it is saved, the editor carries on {}. Cancel keeps the map unsaved and stays here.",
                    then.words().1
                )))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                        this.map_dialog = None;
                        cx.notify();
                    })))
                    .when(overrides, |d| {
                        d.child(button("Override in place", false).on_click(cx.listener(|this, _, _, cx| {
                            this.save_shipped_as(SaveAs::Override, cx);
                            cx.notify();
                        })))
                    })
                    .child(save),
            )
    }

    /// The shipped-map question's answer: saves, then goes where the
    /// switch that asked was going, if the map was written.
    pub(crate) fn save_shipped_as(&mut self, how: SaveAs, cx: &mut Context<Self>) {
        let Some(Dialog::SaveShipped(d)) = &self.map_dialog else {
            return;
        };
        let name = d.name.text.trim().to_owned();
        if how == SaveAs::NewLayout {
            if name.is_empty() {
                return self.message("Name the layout first", true);
            }
            let paths = with_stem(&d.probe, &name);
            if let Some(why) = layout_clash(&self.project.layouts, &name, &paths, None) {
                return self.message(why, true);
            }
        }
        let Some(Dialog::SaveShipped(d)) = self.map_dialog.take() else {
            return;
        };
        if let Some(m) = &mut self.map {
            m.save_as = Some(how);
            if how == SaveAs::NewLayout {
                m.map.name = name;
            }
        }
        if self.save() == Saved::Written
            && let Some(to) = d.then
        {
            self.go(to, false, cx);
        }
    }

    /// Commits a half-typed X or Z before a save or a leave. A value that
    /// isn't a number stays typed, the field keeping the keys.
    pub(crate) fn end_map_typing(&mut self) -> Result<(), String> {
        let Some(m) = &mut self.map else {
            return Ok(());
        };
        let Some(t) = &m.typing else {
            return Ok(());
        };
        let axis = t.field;
        let Some(v) = t.number() else {
            let label = if axis == 0 { "X" } else { "Z" };
            return Err(format!(
                "{label} needs a number of metres, not \"{}\"",
                t.text
            ));
        };
        m.typing = None;
        m.set_position(axis, v.round() as i32);
        self.check_map();
        Ok(())
    }

    /// The Slots pane: the variant's slots, problems first.
    pub fn render_map_slots(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(m) = &self.map else {
            return crate::editor::pane_note("The open map's slots are listed here.");
        };
        let classes = self.maps().map_or(&[][..], |m| m.rules().size_classes);
        let mut order: Vec<usize> = (0..m.slots().len()).collect();
        order.sort_by_key(|&i| m.problems_of(i).next().is_none());
        let rows = order.into_iter().map(|i| {
            let s = &m.slots()[i];
            let what = match m.map.island_of(s) {
                Some(p) => crate::project::island_name(p).to_owned(),
                None if s.role == Role::Marker => "a position".to_owned(),
                None => format!(
                    "pool {}",
                    classes.get(s.class as usize).map_or("?", |c| c.name)
                ),
            };
            let problem = m.problems_of(i).next().map(|p| p.said());
            let label = format!("{} · {what} · {}, {}", role_word(s), s.at[0], s.at[1]);
            div()
                .id(("map-slot", i))
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .cursor_pointer()
                .when(m.selected.has(i), |d| d.bg(theme::get().selected))
                .hover(|d| d.bg(theme::get().hover))
                .child(div().text_size(px(12.)).child(label))
                .children(problem.map(|p| {
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().error)
                        .child(p)
                }))
                .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    if let Some(m) = &mut this.map {
                        let add =
                            e.modifiers().shift || e.modifiers().control || e.modifiers().platform;
                        m.select(Some(i), add);
                    }
                    cx.notify();
                }))
        });
        let rows: Vec<Stateful<Div>> = rows.collect();
        let count = m.slots().len();
        let starts = m.starts().len();
        // What no slot row carries: a variant short of starts, a setup
        // without a variant. A click shows what it names.
        let rules = self.maps().map(|m| m.rules());
        let whole: Vec<(usize, String, bool)> = (m.problems.iter().enumerate())
            .filter(|(_, p)| p.names != Names::Slots)
            .map(|(n, p)| {
                let words = rules.map_or_else(|| p.said(), |r| m.problem_words(p, r));
                (n, words, p.level == Level::Broken)
            })
            .collect();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(widgets::panel_header(format!(
                "{} slots{}",
                count,
                if starts > 0 {
                    format!(" · {starts} start points")
                } else {
                    String::new()
                }
            )))
            .children(
                whole
                    .into_iter()
                    .map(|(n, said, blocks)| problem_row("map-slots-problem", n, said, blocks, cx)),
            )
            .child(
                widgets::vscroll(
                    "map-slots",
                    &self.scrolls.get("map-slots"),
                    div().flex().flex_col().children(rows),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    /// The Inspector: the selected slots, or the map with none.
    pub fn render_map_inspector(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(maps) = self.maps() else {
            return crate::editor::pane_note("A slot's role, size class and island show here.");
        };
        self.list_plays_as(maps);
        let Some(m) = &self.map else {
            return crate::editor::pane_note("A slot's role, size class and island show here.");
        };
        let rules = maps.rules();
        let classes = rules.size_classes;
        let mut body = div().flex().flex_col().gap_2().p_3();
        let picked: Vec<usize> = m.selected.iter().filter(|&i| i < m.slots().len()).collect();
        let Some(&first) = picked.first() else {
            body = body
                .child(section("Map"))
                .child(info_row("Name", &m.map.name))
                .when(!m.map.region.is_empty(), |b| {
                    b.child(info_row(
                        "Region",
                        self.island_rules().province(&m.map.region),
                    ))
                })
                .child(info_row(
                    "Size",
                    format!("{} × {} m", m.size()[0], m.size()[1]),
                ))
                .child(info_row(
                    "Variants",
                    (m.map.variants.iter())
                        .map(|v| variant_label(&m.map.variants, v))
                        .collect::<Vec<_>>()
                        .join(", "),
                ))
                .child(info_row(
                    "Saved as",
                    match (&m.path, m.shipped) {
                        (None, _) => "not yet saved".to_owned(),
                        (Some(p), true) => format!("shipped: {p}"),
                        (Some(p), false) => p.clone(),
                    },
                ))
                .children(self.map_plays_as(cx))
                .children(self.map_layout_options(cx))
                .when(!m.problems.is_empty(), |b| {
                    b.child(section("Problems"))
                        .child(hint("Click one to show what it names"))
                        .children(m.problems.iter().enumerate().map(|(n, p)| {
                            // A warning (a setup without a variant) saves
                            // and installs; level 5 doesn't.
                            let blocks = p.level == Level::Broken;
                            let words = m.problem_words(p, rules);
                            let words = match blocks {
                                true => words,
                                false => format!("{words} (warning)"),
                            };
                            problem_row("map-problem", n, words, blocks, cx)
                        }))
                });
            return div()
                .size_full()
                .bg(theme::get().panel)
                .child(
                    widgets::vscroll("map-inspector", &self.scrolls.get("map-inspector"), body)
                        .size_full(),
                )
                .into_any_element();
        };
        let slot = m.slots()[first];
        let n = picked.len();
        let fixed = slot.fixed();
        let title = if n == 1 {
            "Slot".to_owned()
        } else {
            format!("{n} slots")
        };
        body = body.child(section(&title));
        body = body.child(hint("Role")).child(widgets::dropdown(
            "map-slot-role",
            rules
                .roles
                .iter()
                .map(|(r, _)| (*r, r.label().into()))
                .collect(),
            &slot.role,
            window,
            cx,
            |this, role, _, cx| this.map_set_role(role, cx),
        ));
        if !fixed || rules.pool_slots {
            body = body.child(hint("Size class")).child(widgets::segmented(
                "map-slot-class",
                classes
                    .iter()
                    .enumerate()
                    .map(|(c, k)| (c as u8, k.name.into())),
                &slot.class,
                cx,
                move |this, c, _, cx| {
                    if let Some(m) = &mut this.map {
                        m.set_class(c);
                    }
                    this.map_edited(cx);
                },
            ));
        }
        // Islands that fit the class and suit the role: start islands for
        // a Start slot, the region's own.
        let side_max = classes
            .get(slot.class as usize)
            .or(classes.last())
            .map_or(u32::MAX, |c| c.slot);
        let region = m.map.region.to_ascii_lowercase();
        let mut options: Vec<(String, SharedString)> = Vec::new();
        if rules.pool_slots {
            options.push((String::new(), "Pool: the game picks".into()));
        }
        for i in &self.project.islands {
            options.push((
                i.path.clone(),
                format!("{} (this mod)", crate::project::island_name(&i.path)).into(),
            ));
        }
        for s in self.island_list.as_deref().map_or(&[][..], Vec::as_slice) {
            let fits = s.world[0] <= side_max || !rules.pool_slots;
            let here = region.is_empty() || s.region.eq_ignore_ascii_case(&region);
            let suits = slot.role != Role::Start || s.pool;
            if fits && here && suits && !options.iter().any(|o| o.0 == s.path) {
                options.push((
                    s.path.clone(),
                    format!(
                        "{} · {} m",
                        crate::project::island_name(&s.path),
                        s.world[0]
                    )
                    .into(),
                ));
            }
        }
        let current = m.map.island_of(&slot).unwrap_or("").to_owned();
        if !current.is_empty() && !options.iter().any(|o| o.0 == current) {
            options.push((
                current.clone(),
                crate::project::island_name(&current).into(),
            ));
        }
        body = body.child(hint("Island")).child(widgets::dropdown(
            "map-slot-island",
            options,
            &current,
            window,
            cx,
            |this, path, _, cx| {
                let Some(maps) = this.maps() else { return };
                let facts = (!path.is_empty())
                    .then(|| super::island_facts(this, &path))
                    .flatten();
                if let Some(m) = &mut this.map {
                    let which: Vec<usize> = m.selected.iter().collect();
                    if path.is_empty() {
                        m.back_to_pool(maps.rules());
                    } else {
                        m.assign(&which, &path, facts, maps.rules());
                    }
                }
                this.map_edited(cx);
            },
        ));
        if fixed && rules.pool_slots {
            body = body.child(button("Back to pool", false).on_click(cx.listener(
                |this, _, _, cx| {
                    let Some(maps) = this.maps() else { return };
                    if let Some(m) = &mut this.map {
                        m.back_to_pool(maps.rules());
                    }
                    this.map_edited(cx);
                },
            )));
        }
        if fixed && rules.fixed_turns {
            body = body.child(hint("Turn")).child(widgets::segmented(
                "map-slot-turns",
                [
                    (0u8, "0°".into()),
                    (1, "90°".into()),
                    (2, "180°".into()),
                    (3, "270°".into()),
                ],
                &slot.turns,
                cx,
                |this, t, _, cx| {
                    let maps = this.maps();
                    let first = this.map.as_ref().and_then(|m| m.selected.iter().next());
                    if let (Some(m), Some(maps), Some(i)) = (&mut this.map, maps, first) {
                        let now = m.slots()[i].turns;
                        for _ in 0..(t + 4 - now) % 4 {
                            m.rotate_selected(maps.rules());
                        }
                    }
                    this.map_edited(cx);
                },
            ));
        }
        let grid = rules.grid as f32;
        for (axis, label) in [(0usize, "X (m)"), (1, "Z (m)")] {
            let editing = (m.typing.as_ref())
                .filter(|t| t.field == axis)
                .map(|t| t.display());
            body = body.child(number_field(
                label,
                slot.at[axis] as f32,
                grid,
                editing,
                cx,
                move |this, action, _, cx| {
                    if let Some(m) = &mut this.map {
                        match action {
                            NumberAction::Step(by) => {
                                let at = m.slots().get(first).map_or(0, |s| s.at[axis]);
                                m.set_position(axis, at + by as i32);
                            }
                            NumberAction::Edit => {
                                let at = m.slots().get(first).map_or(0, |s| s.at[axis]);
                                m.typing =
                                    Some(TextEdit::new(axis, at.to_string(), Filter::Number));
                            }
                        }
                    }
                    this.map_edited(cx);
                },
            ));
        }
        let problems: Vec<String> = m.problems_of(first).map(|p| p.said()).collect();
        if !problems.is_empty() {
            body = body
                .child(section("Problems"))
                .children(problems.into_iter().map(|p| {
                    div()
                        .text_size(px(12.))
                        .text_color(theme::get().error)
                        .child(p)
                }));
        }
        if slot.role == Role::Other {
            body = body.child(hint(format!(
                "Type {} in the game's data: kept as it is",
                slot.raw_type
            )));
        }
        div()
            .size_full()
            .bg(theme::get().panel)
            .child(
                widgets::vscroll("map-inspector", &self.scrolls.get("map-inspector"), body)
                    .size_full(),
            )
            .into_any_element()
    }

    /// What each variant may play as, once assets.xml is in: shipped map
    /// assets only exist to replace in games whose layouts replace them.
    pub(crate) fn list_plays_as(&mut self, maps: &dyn anno_core::map::Maps) {
        if maps.rules().registers != Registers::ReplacesShipped {
            return;
        }
        let (Some(m), Some(assets)) = (&mut self.map, self.assets.as_deref()) else {
            return;
        };
        if m.plays_as.is_none() {
            let lists = (m.map.variants.iter())
                .map(|v| maps.plays_as(assets, &m.map, v))
                .collect();
            m.plays_as = Some(lists);
        }
    }

    /// With nothing selected, in a game whose layouts replace shipped maps
    /// (1800): which of them the shown variant plays as, each a checkbox.
    /// The game loads the layout wherever it would load a checked one.
    fn map_plays_as(&self, cx: &mut Context<Self>) -> Option<Div> {
        let maps = self.maps()?;
        if maps.rules().registers != Registers::ReplacesShipped {
            return None;
        }
        let m = self.map.as_ref()?;
        let variant = m.map.variants.get(m.variant)?;
        let label = variant_label(&m.map.variants, variant);
        let mut out = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section(&format!("Plays as · {label}")));
        let Some(lists) = &m.plays_as else {
            return Some(out.child(hint("Listing the shipped maps…")));
        };
        let offered = lists.get(m.variant).map_or(&[][..], Vec::as_slice);
        // Ones it plays as that aren't offered (a layout made before, or a
        // map the assets no longer list) stay, so they can be unchecked.
        let mut rows: Vec<(u32, String)> = (offered.iter())
            .map(|p| (p.guid, p.label.clone()))
            .collect();
        for &g in &variant.replaces {
            if !rows.iter().any(|r| r.0 == g) {
                rows.push((g, format!("Map asset {g}")));
            }
        }
        out = out.child(hint(if m.shipped {
            "The shipped maps of this region and setup it replaces once saved as a new layout"
        } else {
            "The shipped maps of this region and setup the game loads it for; the variant's sea fixes its world size"
        }));
        if rows.is_empty() {
            return Some(out.child(hint("The game lists no map of this region and setup")));
        }
        // Another layout of the project playing as the same map: the
        // project's last redirect wins, so say which.
        let mine = m.path.as_deref();
        let taken = |guid: u32| {
            (self.project.layouts.iter())
                .filter(|l| Some(l.path.as_str()) != mine)
                .find(|l| l.variants.iter().any(|v| v.source_guids.contains(&guid)))
                .map(|l| format!("also {}", l.name))
        };
        for (n, (guid, label)) in rows.into_iter().enumerate() {
            let on = variant.replaces.contains(&guid);
            let detail = taken(guid).unwrap_or_else(|| guid.to_string());
            out = out.child(div().id(("map-plays-as", n)).child(
                widgets::checkbox(label, on, detail).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(m) = &mut this.map {
                            m.set_plays_as(guid, !on);
                        }
                        this.map_edited(cx);
                    },
                )),
            ));
        }
        if variant.replaces.is_empty() {
            out = out.child(hint("Checks none: the game never loads this variant"));
        }
        Some(out)
    }

    /// With nothing selected and the map saved as a project layout: its
    /// Test in game choice.
    fn map_layout_options(&self, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.map.as_ref()?;
        let path = m.path.clone()?;
        let entry = self.project.layouts.iter().find(|l| l.path == path)?;
        let on = entry.fill_with_islands;
        let label = if self.maps()?.rules().registers == Registers::None {
            "Make test map builds on it"
        } else {
            "Test in game fills its pool slots with this mod's islands"
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(section("Testing"))
                .child(widgets::checkbox(label, on, "").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.project.set_layout_test(&path, !on);
                        this.pending_save = true;
                        cx.notify();
                    },
                ))),
        )
    }

    /// The Islands tab: this project's islands, then the shipped ones of
    /// the map's region, each dragged onto a slot or the sea.
    pub fn render_map_islands(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.map_facts_need_list(cx);
        let region = self
            .map
            .as_ref()
            .map(|m| m.map.region.to_ascii_lowercase())
            .unwrap_or_default();
        let mut rows: Vec<(String, String)> = (self.project.islands.iter())
            .map(|i| {
                (
                    i.path.clone(),
                    format!("{} · this mod", crate::project::island_name(&i.path)),
                )
            })
            .collect();
        for s in self.island_list.as_deref().map_or(&[][..], Vec::as_slice) {
            let here = region.is_empty() || s.region.eq_ignore_ascii_case(&region);
            if here && !rows.iter().any(|r| r.0 == s.path) {
                rows.push((
                    s.path.clone(),
                    format!(
                        "{} · {} m",
                        crate::project::island_name(&s.path),
                        s.world[0]
                    ),
                ));
            }
        }
        // Bounded: a 2070 install ships about 400 islands, every region.
        rows.truncate(600);
        // The Library's worker and cache: a picture made for either pane
        // serves both, and none waits for the Library to open.
        self.start_thumbs();
        if let Some(thumbs) = &mut self.thumbs {
            // The worker draws a7m islands from their map picture.
            // TODO(alex): 2070's islands ship none; its codec's `top_view`
            // could draw theirs, as the Library's Islands pane notes too.
            thumbs.request(
                rows.iter()
                    .map(|r| r.0.as_str())
                    .filter(|p| p.ends_with(".a7m")),
            );
        }
        self.poll_thumbs(cx);
        let thumbs = self.thumbs.as_ref();
        let list = div()
            .flex()
            .flex_col()
            .children(rows.into_iter().enumerate().map(|(n, (path, label))| {
                let thumb = thumbs.and_then(|t| t.get_small(&path));
                let drag = label.clone();
                let drop = path.clone();
                div()
                    .id(("map-island", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_grab()
                    .hover(|d| d.bg(theme::get().hover))
                    .child(match thumb {
                        Some(thumb) => img(thumb).size(px(30.)).into_any_element(),
                        None => div().size(px(30.)).into_any_element(),
                    })
                    .child(div().text_size(px(12.)).child(label))
                    .on_drag(IslandDrag(drop), move |_, _, _, cx| {
                        cx.new(|_| widgets::DragLabel(drag.clone().into()))
                    })
                    // A double click puts it in the selected slots.
                    .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                        if e.click_count() < 2 {
                            return;
                        }
                        let Some(maps) = this.maps() else { return };
                        let facts = super::island_facts(this, &path);
                        if let Some(m) = &mut this.map {
                            let which: Vec<usize> = m.selected.iter().collect();
                            m.assign(&which, &path, facts, maps.rules());
                        }
                        this.map_edited(cx);
                    }))
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(div().p_2().child(hint("Drag an island onto a slot or onto open sea; double-click puts it in the selected slots")))
            .child(widgets::vscroll("map-islands", &self.scrolls.get("map-islands"), list).flex_1().min_h_0())
            .into_any_element()
    }

    /// Typing into the inspector's X or Z: Enter applies it.
    pub(crate) fn map_typing_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(m) = &mut self.map else { return false };
        let Some(t) = &mut m.typing else { return false };
        match t.key_down_in(event, cx) {
            EditEvent::Commit => {
                if let Err(e) = self.end_map_typing() {
                    self.message(e, true);
                }
                self.map_edited(cx);
                true
            }
            EditEvent::Cancel => {
                m.typing = None;
                true
            }
            EditEvent::Changed => true,
            EditEvent::Ignored => false,
        }
    }
}

/// One problem as a line that shows what it names when clicked.
fn problem_row(
    id: &'static str,
    n: usize,
    words: String,
    blocks: bool,
    cx: &mut Context<Editor>,
) -> Stateful<Div> {
    div()
        .id((id, n))
        .px_2()
        .py_1()
        .text_size(px(11.))
        .cursor_pointer()
        .hover(|d| d.bg(theme::get().hover))
        .text_color(if blocks {
            theme::get().error
        } else {
            theme::get().highlight
        })
        .child(words)
        .on_click(cx.listener(move |this, _, _, cx| {
            if let Some(m) = &mut this.map {
                m.select_problem(n);
            }
            cx.notify();
        }))
}

fn role_word(s: &Slot) -> String {
    match s.role {
        Role::Other => format!("Type {}", s.raw_type),
        r => r.label().to_owned(),
    }
}

/// A shipped map as one button's words in the New map dialog.
fn shipped_row(s: &ShippedMap, worlds_differ: bool) -> String {
    format!("{} · {}", shipped_name(s), shipped_detail(s, worlds_differ))
}

/// The game's setups offer more than one world size (1800), so a map's
/// world sizes are worth naming.
fn worlds_differ(rules: Option<&anno_core::map::MapRules>) -> bool {
    rules.is_some_and(|r| {
        r.setups
            .iter()
            .any(|s| s.world_size != r.setups[0].world_size)
    })
}

/// A file stem as words: `roman_province_default_01` → "Roman province
/// default 01".
pub(crate) fn pretty_words(stem: &str) -> String {
    let words = stem.replace(['_', '-'], " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A shipped map's name as words, without its region's name in front when
/// the heading over it already says it.
fn shipped_name(s: &ShippedMap) -> String {
    let region = s.region.to_ascii_lowercase();
    let stem = (s.name.strip_prefix(&format!("{region}_")))
        .filter(|_| !region.is_empty())
        .unwrap_or(&s.name);
    pretty_words(stem)
}

/// What a shipped map answers in setup: its size, and its variants' world
/// sizes where the game has several (1800), and island sizes.
fn shipped_detail(s: &ShippedMap, worlds_differ: bool) -> String {
    let mut worlds: Vec<&str> = Vec::new();
    let mut islands: Vec<&str> = Vec::new();
    for v in &s.setups {
        if !v.world_size.is_empty() && !worlds.contains(&v.world_size) {
            worlds.push(v.world_size);
        }
        if !v.island_size.is_empty() && !islands.contains(&v.island_size) {
            islands.push(v.island_size);
        }
    }
    let mut out = Vec::new();
    // The world sizes' maps differ in size; the first's would mislead.
    if worlds.len() <= 1 {
        out.push(format!("{} m", s.size[0]));
    }
    if worlds_differ && !worlds.is_empty() {
        out.push(format!("worlds {}", worlds.join(", ")));
    }
    if !islands.is_empty() {
        out.push(format!("islands {}", islands.join(", ")));
    }
    out.join(" · ")
}

/// A project layout's region and variants, as the Open dialog and the
/// Project overview list it.
pub(crate) fn layout_detail(
    rules: &anno_core::game::IslandRules,
    l: &anno_core::map::LayoutEntry,
) -> String {
    let mut out = Vec::new();
    if !l.region.is_empty() {
        out.push(rules.province(&l.region).to_string());
    }
    // Each size once: 1800's nine variants are three island sizes in each
    // of three world sizes.
    let mut sizes: Vec<&str> = Vec::new();
    for v in &l.variants {
        if !v.island_size.is_empty() && !sizes.contains(&v.island_size.as_str()) {
            sizes.push(&v.island_size);
        }
    }
    if !sizes.is_empty() {
        out.push(format!("islands {}", sizes.join(", ")));
    }
    if sizes.len() != l.variants.len() {
        out.push(crate::project::plural(
            l.variants.len(),
            "variant",
            "variants",
        ));
    }
    out.join(" · ")
}

impl MapEditor {
    /// The field typing goes to, for the platform's text input.
    pub(crate) fn typing_buffer(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        self.typing
            .as_mut()
            .map(|t| t as &mut dyn gpui_editor_kit::field::InputBuffer)
    }
}
