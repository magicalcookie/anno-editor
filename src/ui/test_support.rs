// Shared setup for the UI tests: the installed game, a file open in the
// editor, and the lookups most tests make on it. Game tests return early
// (pass silently) when ANNO117_DIR is unset; they are run with it set.
use super::UiEditor;
use super::bindings::FileTable;
use anno_formats::phx::{Asset, binding::Bindings};
use anno_formats::vfs::Install;
use gpui_editor_kit::gpui::{KeyDownEvent, Keystroke};
use std::sync::Arc;

/// A row of icons with a Text container (Icon, Layout.TextPlane).
pub const ICON_ROW: &str = "5dc6f943-738a-4d18-b2b9-b686277c3d15";
/// A tech-tree template: many textured images, strings and colours.
pub const TECH_TREE: &str = "cdadffd6-13b4-4564-b2ac-f1adde70df9d";
/// A file with a button (AnnoButtonWT, its BtnOverlay_DS) and hover clips.
pub const BUTTON: &str = "ed33b8d5-9c91-4e74-9ab7-35c85be4491b";
/// The QuestTracker template: clips with eased keys.
pub const QUEST_TRACKER: &str = "005e6a6d-92f9-41f4-9951-029bbe1d6f9a";

/// The installed game, or None without ANNO117_DIR.
pub fn install() -> Option<Arc<Install>> {
    let dir = std::env::var_os("ANNO117_DIR")?;
    Some(Arc::new(Install::open(dir).unwrap()))
}

/// The archive path of generated UI file `file` ("questtrackerscene", a
/// guid); a path with a slash is taken as it is.
pub fn path(file: &str) -> String {
    if file.contains('/') {
        file.into()
    } else {
        format!("data/ui/studio/generated/{file}")
    }
}

/// `file` (see [`path`]) open in the editor, or None without the game.
pub fn open(file: &str) -> Option<UiEditor> {
    Some(UiEditor::load(install()?, &path(file)).unwrap())
}

/// The canvas node of the file's own widget called `name` (not one inside
/// an instance of a template).
pub fn node(ui: &UiEditor, name: &str) -> usize {
    (1..ui.nodes.len())
        .find(|&n| &*ui.nodes[n].name == name && !ui.nodes[n].foreign)
        .unwrap_or_else(|| panic!("no widget {name}"))
}

/// Selects the file's own widget `name`, as a canvas click would, and
/// returns its node.
pub fn select(ui: &mut UiEditor, name: &str) -> usize {
    let n = node(ui, name);
    ui.select_widget(n);
    n
}

/// Whether node `n` is the file's own widget placed by its stored
/// Position alone (no layout places it, no binding sets it): a drag or a
/// typed X moves it.
pub fn movable(ui: &UiEditor, n: usize) -> bool {
    let f = &ui.frames[n];
    !ui.nodes[n].foreign
        && f.position_span.is_some()
        && f.placed_by.is_none()
        && f.bound.position.is_none()
}

/// The first number (Float) input the Preview pane lists, as (file,
/// property).
pub fn number_input(ui: &UiEditor) -> (usize, usize) {
    let files = &ui.preview.files;
    (0..files.len())
        .find_map(|f| {
            let p = files[f].props.iter().position(|p| p.kind == 0x03);
            p.map(|p| (f, p))
        })
        .expect("a number input")
}

/// Every slot of `ft` in words, by slot.
pub fn labels(ft: &FileTable) -> Vec<String> {
    (0..ft.table.slots.len()).map(|s| ft.label(s)).collect()
}

/// The first slot of `ft` whose label ends with `end`.
pub fn slot_ending(ft: &FileTable, end: &str) -> usize {
    (0..ft.table.slots.len())
        .find(|&s| ft.label(s).ends_with(end))
        .unwrap_or_else(|| panic!("no slot {end}"))
}

/// The first event slot of `ft` whose label ends with `end`.
pub fn event_ending(ft: &FileTable, end: &str) -> usize {
    (0..ft.table.slots.len())
        .find(|&s| ft.label(s).ends_with(end) && ft.kind(s) == Some(super::bindings::T_EVENT))
        .unwrap_or_else(|| panic!("no event {end}"))
}

/// `asset` saved and parsed again: what the game reads back.
pub fn reopen(asset: &Asset) -> Asset {
    Asset::parse(asset.encode()).unwrap()
}

/// The binding table of `asset` as stored.
pub fn bindings(asset: &Asset) -> Bindings {
    asset.bindings(&asset.widgets())
}

/// Key `k` ("ctrl-z", "delete") pressed once.
pub fn key(k: &'static str) -> KeyDownEvent {
    KeyDownEvent {
        keystroke: Keystroke::parse(k).unwrap(),
        is_held: false,
        prefer_character_input: false,
    }
}

/// The open file's template input called `name` ("-Icon_Size").
pub fn input(ui: &UiEditor, name: &str) -> usize {
    let props = ui.asset.properties();
    (0..props.len())
        .find(|&p| ui.asset.property_name(&props[p]) == name)
        .unwrap_or_else(|| panic!("no input {name}"))
}

/// A library index of one template, called AnnoButtonWT: the one the open
/// file's widget `widget` is a copy of, with that copy as the example a
/// drop adds.
pub fn one_template(ui: &UiEditor, widget: &str) -> super::files::Index {
    use super::files::{Entry, Index};
    let widgets = ui.asset.widgets();
    let w = (0..widgets.len())
        .find(|&w| widgets[w].name.as_deref() == Some(widget))
        .unwrap_or_else(|| panic!("no widget {widget}"));
    let template = ui
        .asset
        .container_template(&widgets[w])
        .expect("a container");
    Index::of(vec![Entry {
        file: template.id.to_string(),
        kind: "WI".into(),
        name: "AnnoButtonWT".into(),
        widgets: 3,
        used_by: 1,
        example: Some((ui.path.rsplit('/').next().unwrap().into(), w as u32)),
        widget_type: None,
    }])
}
