// Owns the Model workspace's Models list, as the Islands list owns the
// Island workspace's: the mod's own models, the ones opened this session,
// and every model the game ships, filed by its folders under `graphics/`.
// A click opens one; resting on one previews it in the Inspector while no
// model is open.

use crate::editor::{Editor, Pane};
use crate::island::pretty;
use crate::project::models::model_name;
use anno_formats::vfs::{FileId, Install};
use gpui_editor_kit::{
    field::{Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme, tree,
    widgets::{self, Disclosure, Tone, hint},
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Rows a search lists at most: "a" matches most of 117's 12,758 models.
const SEARCH_ROWS: usize = 2000;

/// The list's state beside the editor's other panes.
#[derive(Default)]
pub struct List {
    /// The game's models, once listed: every `.cfg` and `.prp` under
    /// `data/`, in folder order.
    pub game: Option<Arc<Vec<String>>>,
    pub loading: bool,
    pub filter: String,
    pub filter_edit: Option<TextEdit<()>>,
    /// Groups the user folded or unfolded against their default: the mod's
    /// and the recent ones start open, the game's closed.
    pub flipped: HashSet<String>,
    pub rows: tree::Tree<()>,
    /// Models whose thumbnails the visible rows asked for, requested on
    /// the next draw: the game ships thousands, and only those in view
    /// are rendered.
    wanted: Rc<RefCell<Vec<String>>>,
    /// The model previewed in the empty Inspector.
    pub peek: Option<String>,
}

/// Every model the game ships, filed: by the two folders under
/// `graphics/`, then by name. A few hundredths of a second over 117's
/// 430k files; built once per install off the UI thread.
pub fn game_models(install: &Install) -> Vec<String> {
    let mut out: Vec<String> = (0..install.file_count())
        .map(|i| FileId(i as u32))
        .filter(|&f| {
            let n = install.file_name(f);
            n.ends_with(".cfg") || n.ends_with(".prp")
        })
        .map(|f| install.file_path(f))
        .filter(|p| p.starts_with("data/"))
        .collect();
    out.sort_by_cached_key(|p| {
        let (first, second) = folders(p);
        (
            first.to_owned(),
            second.to_owned(),
            model_name(p).to_owned(),
            p.clone(),
        )
    });
    out.dedup();
    out
}

/// Where the list files a model: the two folders under `graphics/`
/// (`celtic`, `units`), as 117 and 1800 sort their art; the pack folder
/// (`dlc01`) before `graphics/` goes to the row instead. A model with only
/// one folder above it files under that alone.
fn folders(path: &str) -> (&str, &str) {
    let rest = path.split_once("graphics/").map_or(path, |(_, r)| r);
    let mut parts = rest.split('/');
    match rest.matches('/').count() {
        0 => ("", ""),
        1 => (parts.next().unwrap_or(""), ""),
        _ => (parts.next().unwrap_or(""), parts.next().unwrap_or("")),
    }
}

/// The content pack a model ships in, `None` for the base game:
/// `data/dlc01/...` is "DLC 1", as the Islands list says.
fn pack(path: &str) -> Option<String> {
    crate::browser::pack_name(path.split('/').nth(1)?)
}

/// Where a model is filed, as the list's headings say it: "Celtic ·
/// Buildings".
pub(crate) fn filed(path: &str) -> String {
    match folders(path) {
        (first, "") => heading(first),
        (first, second) => format!("{} · {}", heading(first), heading(second)),
    }
}

/// A folder as a heading: `building_parts` → `Building parts`.
fn heading(folder: &str) -> String {
    let words = folder.replace('_', " ");
    let mut c = words.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => "Other".to_owned(),
    }
}

#[derive(Clone)]
enum Row {
    Model(String),
    Group(String),
    Note,
}

impl List {
    /// Whether the group `key` shows its rows: searching opens them all.
    fn open(&self, key: &str, searching: bool) -> bool {
        let default = matches!(key, "mod" | "recent");
        searching || default != self.flipped.contains(key)
    }
}

impl Editor {
    /// Lists the game's models off the UI thread, once per install.
    fn list_game_models(&mut self, cx: &mut Context<Self>) {
        let list = &mut self.model_list;
        if list.game.is_some() || list.loading {
            return;
        }
        let Some(install) = self.install.clone() else {
            return;
        };
        list.loading = true;
        cx.spawn(async move |this, cx| {
            let models = (cx.background_executor())
                .spawn(async move { game_models(&install) })
                .await;
            this.update(cx, |this, cx| {
                this.model_list.game = Some(Arc::new(models));
                this.model_list.loading = false;
                this.notify_pane(Pane::ModelList, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The mod's models: its cfgs and prps, edited game models and added
    /// ones alike.
    fn mod_models(&self) -> Vec<String> {
        (self.project.file_paths().into_iter())
            .filter(|p| p.ends_with(".cfg") || p.ends_with(".prp"))
            .map(str::to_owned)
            .collect()
    }

    pub fn render_model_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.list_game_models(cx);
        self.start_thumbs();
        // Thumbnails the rows in view asked for last draw.
        let wanted: Vec<String> = self.model_list.wanted.borrow_mut().drain(..).collect();
        if let Some(thumbs) = &mut self.thumbs {
            thumbs.request(wanted.iter().map(String::as_str));
        }
        self.poll_thumbs(cx);
        let words: Vec<String> = (self.model_list.filter.split_whitespace())
            .map(str::to_lowercase)
            .collect();
        let searching = !words.is_empty();
        let asset_words = self.asset_words();
        let name = |path: &str| pretty(asset_words, model_name(path));
        let matches = |path: &str| {
            let hay = format!("{} {}", path.to_lowercase(), name(path).to_lowercase());
            words.iter().all(|w| hay.contains(w.as_str()))
        };
        let open = self.model.as_ref().map(|m| m.path.clone());
        let list = &self.model_list;
        let mut rows: Vec<(Row, tree::Item)> = Vec::new();
        let group =
            |rows: &mut Vec<(Row, tree::Item)>, key: String, label: String, n: usize, depth| {
                let shown = list.open(&key, searching);
                rows.push((
                    Row::Group(key),
                    tree::Item {
                        depth,
                        disclosure: match shown {
                            true => Disclosure::Expanded,
                            false => Disclosure::Collapsed,
                        },
                        label: label.into(),
                        detail: match n {
                            0 => SharedString::default(),
                            n => n.to_string().into(),
                        },
                        selected: false,
                    },
                ));
                shown
            };
        let model = |path: &str, depth| {
            (
                Row::Model(path.to_owned()),
                tree::Item {
                    depth,
                    disclosure: Disclosure::Leaf,
                    label: name(path).into(),
                    detail: pack(path).unwrap_or_default().into(),
                    selected: open.as_deref() == Some(path),
                },
            )
        };
        let own: Vec<String> = self
            .mod_models()
            .into_iter()
            .filter(|p| matches(p))
            .collect();
        if group(&mut rows, "mod".into(), "This mod".into(), own.len(), 0) {
            if own.is_empty() {
                rows.push(note(match searching {
                    true => "None match",
                    false => "None yet: a model you edit and save lands here",
                }));
            }
            rows.extend(own.iter().map(|p| model(p, 1)));
        }
        let recent: Vec<&String> = (self.model_recent.iter()).filter(|p| matches(p)).collect();
        if !recent.is_empty()
            && group(
                &mut rows,
                "recent".into(),
                "Opened recently".into(),
                recent.len(),
                0,
            )
        {
            rows.extend(recent.iter().map(|p| model(p, 1)));
        }
        match list.game.clone() {
            None => rows.push(note("Listing the game’s models…")),
            Some(all) => {
                let mut shown_models = 0;
                let mut i = 0;
                // Runs of one first folder, then of one second folder.
                while i < all.len() {
                    let (first, _) = folders(&all[i]);
                    let end = i + all[i..]
                        .iter()
                        .take_while(|p| folders(p).0 == first)
                        .count();
                    let hits: Vec<&String> = all[i..end].iter().filter(|p| matches(p)).collect();
                    i = end;
                    if hits.is_empty() {
                        continue;
                    }
                    let key = format!("game/{first}");
                    let label = format!("Game · {}", heading(first));
                    if !group(&mut rows, key.clone(), label, hits.len(), 0) {
                        continue;
                    }
                    let mut k = 0;
                    while k < hits.len() {
                        let second = folders(hits[k]).1;
                        let n = hits[k..]
                            .iter()
                            .take_while(|p| folders(p).1 == second)
                            .count();
                        let sub = format!("{key}/{second}");
                        if group(&mut rows, sub, heading(second), n, 1) {
                            for p in &hits[k..k + n] {
                                if shown_models < SEARCH_ROWS {
                                    rows.push(model(p, 2));
                                }
                                shown_models += 1;
                            }
                        }
                        k += n;
                    }
                }
                if shown_models > SEARCH_ROWS {
                    rows.push(note("More match: type more to narrow"));
                }
            }
        }
        let icons: Vec<Option<Arc<RenderImage>>> = match &self.thumbs {
            Some(thumbs) => (rows.iter())
                .map(|(r, _)| match r {
                    Row::Model(path) => thumbs.get_small(path),
                    _ => None,
                })
                .collect(),
            None => Vec::new(),
        };
        let rows = Rc::new(rows);
        let (clicks, headings, peeks, icon_rows) =
            (rows.clone(), rows.clone(), rows.clone(), rows.clone());
        let wanted = self.model_list.wanted.clone();
        let list = tree::List::new(
            "model-list",
            rows.len(),
            move |i| {
                let item = &rows[i].1;
                tree::Item {
                    label: item.label.clone(),
                    detail: item.detail.clone(),
                    ..*item
                }
            },
            move |this: &mut Editor, i, _, cx| match &clicks[i].0 {
                Row::Model(path) => this.open_model(path, cx),
                Row::Group(key) => {
                    if !this.model_list.flipped.remove(key) {
                        this.model_list.flipped.insert(key.clone());
                    }
                    this.notify_pane(Pane::ModelList, cx);
                }
                Row::Note => {}
            },
        )
        // Asked for as the row comes into view, so only those are drawn.
        .icon(move |i| {
            let icon = icons.get(i).cloned().flatten();
            if icon.is_none()
                && let Row::Model(path) = &icon_rows[i].0
            {
                wanted.borrow_mut().push(path.clone());
            }
            icon
        })
        .heading(move |i| match headings[i].0 {
            Row::Note => true,
            Row::Group(_) => headings[i].1.depth > 0,
            Row::Model(_) => false,
        })
        .row_height(32.)
        .decorate({
            let entity = cx.entity().downgrade();
            move |i, row| {
                let Row::Model(path) = &peeks[i].0 else {
                    return row;
                };
                let (path, entity) = (path.clone(), entity.clone());
                row.on_hover(move |hovered, _, cx| {
                    let path = path.clone();
                    let _ = entity.update(cx, |this, cx| {
                        this.after_dwell(path.clone(), *hovered, cx, move |this, cx| {
                            this.peek_model(&path, cx)
                        })
                    });
                })
            }
        })
        .highlight(self.model_list.filter.clone())
        .render(&self.model_list.rows, window, cx);
        let search = text_field(
            "model-list-filter",
            &self.model_list.filter,
            self.model_list.filter_edit.as_ref().map(|e| e.display()),
            "Search models…",
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            let text = this.model_list.filter.clone();
            this.model_list.filter_edit = Some(TextEdit::new((), text, Filter::Text));
            cx.notify();
        }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(div().m_1().child(search))
            .child(div().flex_1().min_h_0().child(list.size_full()))
            .into_any_element()
    }

    /// Previews `path` in the empty Inspector, as the Islands list does.
    fn peek_model(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.model.is_some() || self.model_list.peek.as_deref() == Some(path) {
            return;
        }
        self.model_list.peek = Some(path.to_owned());
        if let Some(thumbs) = &mut self.thumbs {
            thumbs.request([path]);
        }
        self.poll_thumbs(cx);
        self.notify_pane(Pane::ModelInspector, cx);
    }

    /// The Inspector while no model is open: the hovered model's picture,
    /// name and path with Open, or what pointing at one does.
    pub(crate) fn render_model_peek(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(path) = self.model_list.peek.clone() else {
            return crate::editor::pane_note(
                "Point at a model in the Models list to preview it here.",
            );
        };
        let picture = self.thumbs.as_ref().and_then(|t| t.get(&path));
        let filed = filed(&path);
        let pack = pack(&path).unwrap_or_else(|| "Base game".to_owned());
        div()
            .size_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .bg(theme::get().panel)
            .child(
                div()
                    .w_full()
                    .h(px(220.))
                    .rounded(px(4.))
                    .bg(theme::get().well)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match picture {
                        Some(image) => img(image).max_w_full().max_h_full().into_any_element(),
                        None => hint("Rendering…").into_any_element(),
                    }),
            )
            .child(
                div()
                    .pt_1()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(pretty(self.asset_words(), model_name(&path))),
            )
            .child(hint(format!("{filed} · {pack}")))
            .child(hint(path.clone()))
            // Open, and the file among its neighbours, as the Islands
            // list's preview offers.
            .child({
                let shown = path.clone();
                div()
                    .pt_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(widgets::action("Open", Tone::Primary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.open_model(&path, cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        div()
                            .id("model-peek-show-in-assets")
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
            .into_any_element()
    }
}

fn note(text: &'static str) -> (Row, tree::Item) {
    (
        Row::Note,
        tree::Item {
            depth: 1,
            disclosure: Disclosure::Leaf,
            label: text.into(),
            detail: SharedString::default(),
            selected: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{folders, heading};

    #[test]
    fn models_file_under_their_two_folders_below_graphics() {
        let worker = "data/base/graphics/celtic/units/inhabitants/production/worker_lean_f_01/worker_lean_f_01.cfg";
        assert_eq!(folders(worker), ("celtic", "units"));
        assert_eq!(
            folders("data/cdlc04/graphics/vehicles/airship/airship.cfg"),
            ("vehicles", "airship")
        );
        assert_eq!(folders("data/graphics/odd.cfg"), ("", ""));
        assert_eq!(folders("data/graphics/props/a.cfg"), ("props", ""));
        assert_eq!(heading("building_parts"), "Building parts");
    }
}
