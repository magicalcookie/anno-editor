// Owns the UI workspace: one compiled Phoenix asset, its decoded records,
// the canvas (`canvas`, laid out by `scene`), the inspector, and value
// edits: scalar property values, text, colors, custom property defaults.
// The widget tree pane and structural edits live in `hierarchy`, the UI file
// index in `files`.
mod bindings;
mod canvas;
pub mod files;
mod game_view;
mod gpu;
mod graph;
mod hierarchy;
mod interact;
pub mod library;
mod placement;
mod preview;
mod scene;
mod states;
#[cfg(test)]
mod test_support;
mod timeline;

use crate::{
    editor::{Editor, Workspace, empty_pane},
    project::Project,
};
use anno_formats::{
    phx::{self, Asset, DefaultValue, Edit, Names, Record, Value},
    vfs::Install,
};
use anyhow::{Result, bail};
use gpui_editor_kit::{
    design::Handle,
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    history::History,
    theme,
    tree::{self, Tree},
    widgets::{Disclosure, button, info_row, section},
};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// Folder of compiled widget/component assets, keyed by guid.
const GENERATED: &str = "data/ui/studio/generated/";
/// A load that panicked in the layout code, told apart from ordinary errors.
const LAYOUT_UNSUPPORTED: &str = "layout not supported";

/// Built-in names, which embed the harvested corpus. `ANNO_PHX_NAMES` points
/// at a fresher `phx_names_corpus.json` to merge on top while harvesting.
fn names() -> &'static Names {
    static NAMES: OnceLock<Names> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = Names::builtin();
        if let Some(path) = std::env::var_os("ANNO_PHX_NAMES")
            && let Err(e) = names.load_corpus(&PathBuf::from(path))
        {
            eprintln!("PHX names: {e:#}");
        }
        names
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A laid-out widget, by canvas node index.
    Widget(usize),
    Node(usize),
    Record(usize),
    Property(usize),
}

/// The screen a template is being edited inside: it stays the layout's
/// root and keeps its own edits and undo steps.
struct Root {
    asset: Asset,
    history: History<Asset>,
    path: String,
}

pub struct UiEditor {
    /// The file being edited: the screen, or a template edited in place
    /// inside it (`scope`).
    pub path: String,
    asset: Asset,
    history: History<Asset>,
    /// While a template is edited in place: the screen around it.
    root: Option<Root>,
    /// The template instance edited in place: its file and its instance
    /// key in the screen's layout.
    scope: Option<(anno_formats::phx::Guid, u64)>,
    /// Templates edited in place and left again, with their undo steps;
    /// they show and save as edited.
    edited: std::collections::HashMap<anno_formats::phx::Guid, (Asset, History<Asset>)>,
    /// Per node, inside the scope's instance (a template's widgets and
    /// what they contain); empty without a scope.
    in_scope: Bits,
    /// Which file each undo step, oldest first, was taken in: the screen
    /// (None) or a template edited in place at an instance. Undo goes back
    /// through them in order, whatever is being edited now; each file
    /// keeps its own snapshots.
    journal: Vec<Step>,
    redo_journal: Vec<Step>,
    /// Expansion of the records view, by asset node.
    tree: Tree<usize>,
    /// Expansion of the widget tree, by canvas node.
    widget_tree: Tree<usize>,
    /// The widget tree's search text (see `widget_rows`).
    tree_filter: String,
    /// Plain shipped widgets the Add buttons copy, by class.
    prototypes: std::collections::HashMap<u32, phx::WidgetCopy>,
    selected: Option<usize>,
    /// The first layout framed the drawn widgets; later ones leave the
    /// view where it is.
    content_framed: bool,
    show_opaque: bool,
    /// Raw object/record rows instead of the widget tree.
    show_records: bool,
    editing: Option<TextEdit<usize>>,
    /// A number of the inspector's Layout block being typed.
    layout_edit: Option<TextEdit<placement::Field>>,
    /// The widget (of this file) the Layout number is being typed for.
    layout_target: Option<usize>,
    /// Anchor and pivot changes adjust Position so the widget stays put.
    keep_in_place: bool,
    /// The canvas pivot handle is being dragged.
    pivot_drag: bool,
    install: Arc<Install>,
    images: scene::Images,
    /// English UI text by LineId, for LocalizationIds.
    texts: Arc<std::collections::HashMap<i64, String>>,
    nodes: Vec<canvas::Node>,
    /// Each node's parent node, from the depths.
    parents: Vec<Option<usize>>,
    frames: Vec<scene::Frame>,
    /// Alternative groups sharing a spot; one shows at a time.
    state_groups: Vec<states::StateGroup>,
    /// Shown member per state group, keyed by the group's first member.
    chosen_states: std::collections::HashMap<usize, usize>,
    /// Draw blockers, scroll bars and other widgets the game places itself.
    show_helpers: bool,
    /// Evaluate the templates' data bindings over the stored values.
    bindings: bool,
    /// Show screens as opened: opening clips at their end.
    shown: bool,
    /// Fill unset template properties with plausible game values.
    sample: bool,
    /// The canvas simulates input (hover, press) instead of editing.
    interact: bool,
    /// Interaction states held on template instances.
    sims: Vec<anno_formats::phx::layout::Sim>,
    /// Other screens stacked around this one, and the reference shot.
    game: game_view::GameView,
    /// Timeline clips and playback.
    player: timeline::Player,
    /// Fake game data for template properties.
    preview: preview::Preview,
    /// The Bindings pane lists the whole file, not only the selection's.
    bindings_all: bool,
    /// The Bindings pane's graph: on or not, pan and zoom.
    graph: graph::view::GraphView,
    /// The pane keys go to after the last click (see [`Keys`]).
    pub(crate) keys: Keys,
    /// Esc in the graph or timeline asked to give the keys back to the
    /// canvas; the editor decides (`Editor::hand_back`), since only the
    /// dock knows whether the canvas shows.
    pub(crate) hand_back: bool,
    /// Counts relayouts: what was worked out from one layout (see
    /// `file_table`, the graph) is reused until the next.
    layout_gen: u64,
    /// Edits, undos and scope changes so far, and the one the last layout
    /// saw: binding, slot and graph node indices hold between two of these.
    /// Layouts that only re-evaluate (playback frames, preview values,
    /// game data flags) keep them, so a menu stays open while a clip plays.
    revision: u64,
    laid_out: u64,
    /// The last `file_table`: (file, layout_gen) it was built for. The
    /// Bindings pane, the graph and the Data section each want it every
    /// frame, and building one clones the asset and reads every binding.
    table_cache: Option<bindings::TableCache>,
    /// Files' names as the UI files list shows them, by guid (filled once
    /// its index is in): what messages call templates.
    names: std::sync::Arc<std::collections::HashMap<anno_formats::phx::Guid, String>>,
    /// A binding being put together in the inspector's Data section.
    binder: Option<bindings::Binder>,
    /// Game data the install's templates bind, for the binder.
    catalog: Option<Arc<bindings::Catalog>>,
    canvas: canvas::Canvas,
    /// Last pointer position over the canvas, for drag previews.
    cursor: Point<Pixels>,
    canvas_bounds: Bounds<Pixels>,
    /// Draws the canvas's images (see `gpu`).
    gpu: gpu::Renderer,
    pub message: Option<(String, bool)>,
}

/// Which pane of the UI workspace keys go to after a click (a text field
/// being typed in takes them before any). One owner, set by every click
/// that takes keys (`Editor::keys_to`): two flags, each cleared by some
/// handlers only, let the graph keep the keys of a timeline click.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Keys {
    #[default]
    Canvas,
    Graph,
    Timeline,
    /// A pane the UI editor does not own (the asset browser and preview
    /// shown in the UI workspace): no widget command acts on a key while
    /// it has them (round 10: Delete in the browser's search deleted the
    /// selected container).
    Other,
}

/// Shortcuts the editor keeps wherever the keys are: undo, redo, save,
/// workspaces, new and open project, quit, install, playtest. A pane that
/// holds the keys (graph, timeline) passes these on and stops every other
/// chord.
pub(crate) fn editor_shortcut(k: &gpui_editor_kit::gpui::Keystroke) -> bool {
    let m = k.modifiers;
    let ctrl = m.control || m.platform;
    match k.key.as_str() {
        "f5" => true,
        "z" | "y" | "s" | "0" | "1" | "2" | "3" | "q" | "n" | "o" => ctrl,
        "u" => ctrl && m.shift,
        _ => false,
    }
}

/// Whether a key stops at the field being typed in when the field did not
/// take it: every chord but the editor's own (round 9: Ctrl+D while typing
/// a clip name or Position X duplicated the selected widget).
pub(crate) fn field_keeps(k: &gpui_editor_kit::gpui::Keystroke) -> bool {
    let m = k.modifiers;
    (m.control || m.platform || m.alt) && !editor_shortcut(k)
}

/// One undo step: the file it changed (None: the screen) and the widget
/// selected when it was made, which undoing it selects again (round 12:
/// undoing a delete selected the neighbour, not the widget it brought
/// back).
#[derive(Clone, Copy, Debug)]
struct Step {
    scope: Option<(anno_formats::phx::Guid, u64)>,
    selected: Option<phx::ObjectId>,
}

/// What the editor's window tests (editor/tests.rs) reach past the UI.
#[cfg(test)]
impl UiEditor {
    /// Selects the file's own widget called `name`, as a canvas click would.
    pub(crate) fn select_named(&mut self, name: &str) {
        test_support::select(self, name);
    }

    pub(crate) fn widget_count(&self) -> usize {
        self.asset.widgets().len()
    }

    /// Types `text` into the selected widget's Position X, not applied.
    pub(crate) fn type_position_x(&mut self, text: &str) {
        self.start_layout_edit(placement::Field::Position(0));
        self.layout_edit.as_mut().unwrap().text = text.into();
    }

    /// The Bindings pane draws the graph, not the List.
    pub(crate) fn show_graph(&mut self) {
        self.graph.on = true;
    }

    /// The text of the record field being typed (a pin's fixed value).
    pub(crate) fn typed_record(&self) -> Option<String> {
        self.editing.as_ref().map(|e| e.text.clone())
    }

    /// The text Position X (or another layout field) shows while typed.
    pub(crate) fn typed(&self) -> Option<String> {
        self.layout_edit.as_ref().map(|e| e.text.clone())
    }

    /// The selected widget's stored Position X.
    pub(crate) fn position_x(&self) -> f32 {
        let n = self.canvas.selected.unwrap();
        self.frames[n].position[0]
    }
}

impl UiEditor {
    /// Keys go to `keys` from now on, and to no field: half-typed values
    /// (layout, record, preview) are committed as a click elsewhere always
    /// did, searches and filters (they apply as typed) stop taking keys. A
    /// field starts its own edit after this.
    pub(crate) fn take_keys(&mut self, keys: Keys) {
        // A value the field refuses goes, with why: the keys follow the
        // click, not the field.
        if let Err(e) = self.commit_typed() {
            self.message = Some((format!("Not applied: {e:#}"), true));
            self.drop_typed();
        }
        if let Some(b) = &mut self.binder {
            b.edit = None;
        }
        self.keys = keys;
    }

    /// Commits the value being typed in any field (layout, record, preview);
    /// one the field refuses stays as typed, the field still taking keys.
    fn commit_typed(&mut self) -> Result<()> {
        self.commit_layout_edit()?;
        self.commit_editing()?;
        self.commit_preview()
    }

    /// Lets go of what any field holds typed, applying nothing.
    fn drop_typed(&mut self) {
        self.layout_edit = None;
        self.layout_target = None;
        self.editing = None;
        self.preview.edit = None;
    }

    /// Applies the half-typed value before a save, the keys staying where
    /// they are. A value the field refuses stays as typed, with the field
    /// still taking keys, and the error comes back: the save waits for it
    /// (round 12: "-" in Position X was dropped, its error cleared, and the
    /// status said "Saved 0 documents"). A click elsewhere still drops it.
    pub(crate) fn apply_typing(&mut self) -> Result<(), String> {
        self.commit_typed().map_err(|e| format!("{e:#}"))?;
        // What the save did is the news, not "Edited".
        self.message = None;
        Ok(())
    }

    /// The field keys go to, in the order its key handler tries them.
    pub(crate) fn typing(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        if self.layout_edit.is_some() {
            return self
                .layout_edit
                .as_mut()
                .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer);
        }
        if self.preview.typing().is_some() {
            return self.preview.typing();
        }
        if let Some(e) = self.binder.as_mut().and_then(|b| b.edit.as_mut()) {
            return Some(e as &mut dyn gpui_editor_kit::field::InputBuffer);
        }
        self.editing
            .as_mut()
            .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer)
    }
}

impl UiEditor {
    fn load(install: Arc<Install>, path: &str) -> Result<Self> {
        let asset = Asset::parse(install.read(path)?)?;
        // Enum entry names and widget types live only in the shipped
        // Phoenix sources (30 small files); everything shown after this
        // reads them (`short`, the widget library).
        anno_formats::phx::register_sources(&install);
        let texts = Arc::new(load_texts(&install));
        let mut tree = Tree::default();
        tree.expand(0);
        let mut editor = Self {
            path: path.to_owned(),
            asset,
            history: History::new(true),
            root: None,
            scope: None,
            edited: Default::default(),
            in_scope: Bits::default(),
            journal: Vec::new(),
            redo_journal: Vec::new(),
            tree,
            widget_tree: Tree::default(),
            tree_filter: String::new(),
            prototypes: Default::default(),
            selected: None,
            content_framed: false,
            show_opaque: false,
            show_records: false,
            editing: None,
            layout_edit: None,
            layout_target: None,
            keep_in_place: true,
            pivot_drag: false,
            install,
            images: scene::Images::default(),
            texts,
            nodes: Vec::new(),
            parents: Vec::new(),
            frames: Vec::new(),
            state_groups: Vec::new(),
            chosen_states: std::collections::HashMap::new(),
            show_helpers: false,
            bindings: true,
            shown: true,
            sample: true,
            interact: false,
            sims: Vec::new(),
            game: Default::default(),
            player: timeline::Player {
                speed: 1.,
                ..Default::default()
            },
            preview: Default::default(),
            bindings_all: false,
            graph: Default::default(),
            keys: Keys::Canvas,
            hand_back: false,
            names: Default::default(),
            layout_gen: 0,
            revision: 0,
            laid_out: 0,
            table_cache: None,
            binder: None,
            catalog: None,
            canvas: canvas::Canvas::default(),
            cursor: point(px(0.), px(0.)),
            canvas_bounds: Bounds::default(),
            gpu: gpu::Renderer::default(),
            message: None,
        };
        editor.collect_clips();
        editor.relayout();
        editor.reset_tree();
        Ok(editor)
    }

    /// Commits whatever is being typed into the inspector to the widget or
    /// value it was typed for: before the selection, the edited file or
    /// the scope change, so typed text never lands somewhere else.
    pub(super) fn flush_edits(&mut self) {
        if let Err(e) = self
            .commit_layout_edit()
            .and_then(|()| self.commit_editing())
        {
            self.message = Some((format!("Not applied: {e:#}"), true));
            self.layout_edit = None;
            self.layout_target = None;
            self.editing = None;
        }
    }

    /// Commits the record value being typed (a pin's fixed value, a List
    /// chip, a label's text) when it changed; one the field refuses stays
    /// as typed.
    fn commit_editing(&mut self) -> Result<()> {
        let Some(edit) = self.editing.take() else {
            return Ok(());
        };
        if edit.field >= self.asset.spans().len()
            || self.edit_text(edit.field).is_none_or(|t| t == edit.text)
        {
            return Ok(());
        }
        let r = self.commit(edit.field, &edit.text);
        if r.is_err() {
            self.editing = Some(edit);
        }
        r
    }

    fn select_widget(&mut self, node: usize) {
        self.flush_edits();
        // Selecting inside a hidden state shows that state.
        let mut changed = false;
        let mut at = Some(node);
        while let Some(i) = at {
            if let Some((group, false)) =
                states::state_of(&self.state_groups, &self.chosen_states, i)
            {
                self.chosen_states.insert(group, i);
                changed = true;
            }
            at = (0..i)
                .rev()
                .find(|&p| self.nodes[p].depth < self.nodes[i].depth);
        }
        if changed {
            self.relayout();
        }
        self.canvas.selected = Some(node);
        self.reveal(node);
        self.selected = if self.nodes[node].foreign {
            None
        } else {
            self.asset
                .widgets()
                .get(self.nodes[node].widget)
                .map(|w| w.object)
        };
    }

    /// Lays the widget tree out again after any change to the asset.
    fn relayout(&mut self) {
        // Everything derived from a layout (binding tables with traces, the
        // graph) is good until the next one.
        self.layout_gen += 1;
        // UI that holds binding or node indices goes with the revision it
        // was opened on: a remove menu left open over an undo removed
        // another binding, or one past the end.
        let revised = std::mem::replace(&mut self.laid_out, self.revision) != self.revision;
        if revised {
            // The graph drawn keeps its places through any edit, undo or
            // redo, from any pane: its cache is still what is on screen.
            // Round 7: holding only in the graph's own edits let an undo
            // reflow the view the edit was not made in.
            self.graph.hold_places();
            self.graph.forget_indices();
        }
        let drawn = self.nodes.len();
        // Who the selection, hover, press and layer menu are, to find them
        // again below: a flag or a list that shows more widgets renumbers
        // the nodes (round 15: after a flag the inspector moved from the
        // selected button to an InfoTip that came into view before it).
        let key =
            |n: usize| (n < self.nodes.len()).then(|| node_key(&self.nodes, &self.parents, n));
        let kept_selected = self.canvas.selected.and_then(key);
        let kept_hover = self.canvas.hover.and_then(key);
        let kept_press = self.canvas.press.and_then(|p| p.node).and_then(key);
        let kept_menu: Vec<Option<NodeKey>> = self
            .canvas
            .menu
            .as_ref()
            .map(|m| m.stack.iter().map(|&n| key(n)).collect())
            .unwrap_or_default();
        // In place, the edited template stands in for the shipped one. Its
        // instances in the screen's last layout are rebuilt; the rest of
        // the screen comes over as it was.
        let mut stale = Vec::new();
        if let Some((g, _)) = self.scope {
            let root = self.root.as_ref().and_then(|r| r.asset.guid());
            let before = self.images.instances_of(root, g);
            if self.images.put(g, &self.asset) {
                match before {
                    Some(keys) => stale = keys,
                    None => self.images.forget(root),
                }
            }
        }
        let scope = self.scope;
        let asset = self.root.as_ref().map_or(&self.asset, |r| &r.asset);
        let design = self.canvas.design_view;
        // "This file" of clips and held states is the edited template.
        let own = |file: Option<anno_formats::phx::Guid>| file.or(scope.map(|s| s.0));
        let (nodes, frames) = scene::layout(
            asset,
            &self.install,
            &mut self.images,
            &self.texts,
            scene::View {
                design,
                bindings: self.bindings,
                play: self.player.play().map(|p| phx::layout::Play {
                    file: own(p.file),
                    ..p
                }),
                preview: self.preview.values.clone(),
                shown: self.shown,
                sims: self
                    .sims
                    .iter()
                    .map(|s| phx::layout::Sim {
                        file: own(s.file),
                        ..*s
                    })
                    .collect(),
                sample: self.sample,
                edit: scope,
                stale,
            },
            |crc| {
                asset
                    .name(names(), crc)
                    .map_or_else(|| format!("{crc:08x}"), str::to_owned)
            },
        );
        if let Some(root) = nodes.first() {
            self.canvas
                .view
                .resize([root.rect[2].max(1.), root.rect[3].max(1.)]);
        }
        let mut nodes = nodes;
        // With bindings on, the templates' own bindings pick which state
        // shows (the quest tracker's lists switch on isEmpty); guessing
        // alternatives from overlapping boxes would hide live widgets such
        // as the archive buttons, which share a spot with their InfoTip.
        self.state_groups = if self.bindings {
            Vec::new()
        } else {
            states::groups(&nodes)
        };
        states::apply(
            &mut nodes,
            &self.state_groups,
            &self.chosen_states,
            self.show_helpers,
        );
        self.parents = states::parents(&nodes);
        // In place: the template's widgets and everything under them.
        self.in_scope = if self.scope.is_some() {
            let mut inside = Bits::new(nodes.len());
            for (i, n) in nodes.iter().enumerate() {
                let on = !n.foreign || self.parents[i].is_some_and(|p| inside.get(p));
                inside.set(i, on);
            }
            inside
        } else {
            Bits::default()
        };
        // Outlines name canvas nodes: a list that grew or shrank (sample
        // items, a flag) renumbers them.
        if revised || nodes.len() != drawn {
            self.canvas.marked.clear();
        }
        self.nodes = nodes;
        self.frames = frames;
        // Open on what draws: a screen that is a strip at the page's foot
        // (a construction bar) fills the view instead of a sliver of it.
        // Home goes back to the whole page.
        if !self.content_framed {
            let [pw, ph] = self.canvas.view.size;
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for n in self.nodes.iter().skip(1) {
                let [x, y, w, h] = n.rect;
                // Backdrops the size of the page (often transparent) say
                // nothing about where the artwork is.
                // By kind: images are still loading on the first layout.
                // Hidden parts (closed popups) would frame the whole page.
                if matches!(n.kind, canvas::Kind::Container)
                    || !n.visible
                    || n.opacity < 0.02
                    || w < 2.
                    || h < 2.
                    || w * h > 0.7 * pw * ph
                {
                    continue;
                }
                (x0, y0) = (x0.min(x.max(0.)), y0.min(y.max(0.)));
                (x1, y1) = (x1.max((x + w).min(pw)), y1.max((y + h).min(ph)));
            }
            let area = (x1 - x0).max(0.) * (y1 - y0).max(0.);
            // Until something draws (template widgets arrive after the
            // first layout), keep looking.
            if x1 > x0 {
                self.content_framed = true;
                // Only when it brings the artwork closer. A strip as wide as
                // the page (the construction menu's bar) frames too: at the
                // page's zoom it was a sliver at the foot.
                self.canvas.view.region =
                    (area < 0.6 * pw * ph).then_some([x0, y0, x1 - x0, y1 - y0]);
                self.canvas.view.refit();
            }
        }
        let find = |was: Option<usize>, key: &Option<NodeKey>| {
            let key = key.as_ref()?;
            let at = |i: usize| node_key(&self.nodes, &self.parents, i) == *key;
            // Mostly where it was; else wherever it went, or gone.
            was.filter(|&i| i < self.nodes.len() && at(i))
                .or_else(|| (0..self.nodes.len()).find(|&i| at(i)))
        };
        self.canvas.selected = find(self.canvas.selected, &kept_selected);
        self.canvas.hover = find(self.canvas.hover, &kept_hover);
        if let Some(p) = self.canvas.press
            && let Some(was) = p.node
        {
            match find(Some(was), &kept_press) {
                Some(n) => self.canvas.press = Some(canvas::Press { node: Some(n), ..p }),
                None => {
                    if p.dragging {
                        self.canvas.view.cancel();
                    }
                    self.canvas.press = None;
                }
            }
        }
        if let Some(m) = &mut self.canvas.menu {
            let stack = std::mem::take(&mut m.stack);
            m.stack = stack
                .into_iter()
                .zip(&kept_menu)
                .filter_map(|(n, key)| find(Some(n), key))
                .collect();
        }
        if gpui_editor_kit::profile::enabled() {
            eprintln!("{}", self.images.footprint());
        }
        let _ = release_heap(false);
    }

    /// Applies edits as one undo step.
    fn apply(&mut self, edits: &[Edit]) -> Result<()> {
        self.ensure_editable()?;
        let mut asset = self.asset.clone();
        for edit in edits {
            asset = asset.apply(edit)?.asset;
        }
        self.checkpoint();
        self.asset = asset;
        self.relayout();
        Ok(())
    }

    /// Replaces the edited file with `next` after a structural edit (a
    /// binding or a track added or removed: slot and clip indices move), as
    /// one undo step.
    fn replace_asset(&mut self, next: Asset) {
        self.checkpoint();
        self.asset = next;
        self.images.forget(self.asset.guid());
        self.collect_clips();
        self.relayout();
    }

    /// A finished canvas move/resize becomes Position/Scale edits.
    fn canvas_change(&mut self, change: canvas::Change) {
        let Some(node) = self
            .nodes
            .iter()
            .position(|n| n.widget == change.widget && !n.foreign)
        else {
            self.message = Some((
                "That widget comes from a container’s template file; open the template to edit it"
                    .into(),
                true,
            ));
            return;
        };
        let result = scene::edits_for(&self.frames[node], change.before, change.after)
            .map_err(anyhow::Error::msg)
            .and_then(|edits| {
                // Recording an animation: the change becomes keys.
                if self.record_change(node, &edits) {
                    return Ok(false);
                }
                self.apply(&edits).map(|()| true)
            });
        match result {
            Ok(true) => self.message = Some((format!("Moved {}", self.nodes[node].name), false)),
            Ok(false) => {}
            Err(e) => self.message = Some((format!("Not applied: {e:#}"), true)),
        }
    }

    pub fn dirty(&self) -> bool {
        self.history.dirty()
            || self.root.as_ref().is_some_and(|r| r.history.dirty())
            || self.edited.values().any(|e| e.1.dirty())
    }

    /// Template `g` as the UI files list names it, else by its top widget,
    /// else by the start of its guid.
    pub(super) fn template_name(&self, g: anno_formats::phx::Guid) -> String {
        if let Some(n) = self.names.get(&g) {
            return n.clone();
        }
        let top = self
            .images
            .templates_loaded(g)
            .and_then(|a| a.widgets().first().and_then(|w| w.name.clone()));
        top.unwrap_or_else(|| g.to_string()[..8].to_owned())
    }

    /// Whether node `i` belongs to what is being edited: everything, or
    /// with a template open in place, its widgets.
    pub(super) fn scoped(&self, i: usize) -> bool {
        self.in_scope.is_empty() || self.in_scope.get(i)
    }

    /// Files with edits Save has not written yet: the screen (None) and
    /// templates edited in place, each with how many copies of it this
    /// screen shows and one to edit it at.
    pub(super) fn unsaved(&self) -> Vec<(Option<anno_formats::phx::Guid>, usize, Option<u64>)> {
        let screen_dirty = match &self.root {
            Some(r) => r.history.dirty(),
            None => self.history.dirty(),
        };
        let mut files: Vec<Option<anno_formats::phx::Guid>> = Vec::new();
        if screen_dirty {
            files.push(None);
        }
        if let Some((g, _)) = self.scope
            && self.history.dirty()
        {
            files.push(Some(g));
        }
        let mut more: Vec<_> = self
            .edited
            .iter()
            .filter(|e| e.1.1.dirty())
            .map(|e| Some(*e.0))
            .collect();
        more.sort_unstable();
        files.extend(more);
        files
            .into_iter()
            .map(|f| {
                let Some(g) = f else { return (None, 1, None) };
                let mut keys: Vec<u64> = self
                    .nodes
                    .iter()
                    .flat_map(|n| n.owners.iter())
                    .filter(|o| o.0 == g)
                    .map(|o| o.1)
                    .collect();
                keys.sort_unstable();
                keys.dedup();
                // In place, the template's own copy is the edited one.
                let at = self
                    .scope
                    .filter(|s| s.0 == g)
                    .map(|s| s.1)
                    .or(keys.first().copied());
                (Some(g), keys.len().max(1), at)
            })
            .collect()
    }

    /// The asset the layout is built from: the screen, also while a
    /// template is edited in place.
    pub(super) fn layout_root(&self) -> &Asset {
        self.root.as_ref().map_or(&self.asset, |r| &r.asset)
    }

    /// Whether `file` (None: the screen) is the file being edited.
    pub(super) fn is_edited_file(&self, file: Option<anno_formats::phx::Guid>) -> bool {
        file == self.scope.map(|s| s.0)
    }

    /// Edits template `g` in place, at its instance `key` in the screen's
    /// layout: the screen stays drawn around it, and the inspector, tree,
    /// bindings and timeline work on the template.
    pub fn enter_template(&mut self, g: anno_formats::phx::Guid, key: u64) -> Result<String> {
        if self.scope == Some((g, key)) {
            return Ok(String::new());
        }
        // Every field applies where it was typed, Preview included (round
        // 12: its value went on typing into the next scope's inputs).
        self.take_keys(self.keys);
        let (asset, history) = match self.edited.remove(&g) {
            Some(e) => e,
            None => {
                let a = self
                    .images
                    .asset(&self.install, g)
                    .ok_or_else(|| anyhow::anyhow!("template {g} is not in the install"))?;
                ((*a).clone(), History::new(true))
            }
        };
        let old_asset = std::mem::replace(&mut self.asset, asset);
        let old_history = std::mem::replace(&mut self.history, history);
        let old_path = std::mem::replace(&mut self.path, format!("{GENERATED}{g}"));
        match self.scope.replace((g, key)) {
            // From another template: it keeps its edits for later.
            Some((was, _)) => {
                self.edited.insert(was, (old_asset, old_history));
            }
            None => {
                self.root = Some(Root {
                    asset: old_asset,
                    history: old_history,
                    path: old_path,
                });
            }
        }
        self.scope_changed();
        Ok(
            "Editing the template in place: changes reach every place that uses it. Esc steps out"
                .into(),
        )
    }

    /// Back to editing the screen itself.
    pub fn leave_template(&mut self) {
        if self.scope.is_none() {
            return;
        }
        self.take_keys(self.keys);
        let (Some((g, _)), Some(root)) = (self.scope.take(), self.root.take()) else {
            return;
        };
        let asset = std::mem::replace(&mut self.asset, root.asset);
        let history = std::mem::replace(&mut self.history, root.history);
        self.path = root.path;
        self.edited.insert(g, (asset, history));
        self.scope_changed();
    }

    fn scope_changed(&mut self) {
        self.revision += 1;
        self.graph.pick = None;
        self.editing = None;
        self.layout_edit = None;
        // Its (file, input) indices name the old scope's inputs.
        self.preview.edit = None;
        self.binder = None;
        self.canvas.selected = None;
        self.canvas.hover = None;
        self.canvas.press = None;
        self.selected = None;
        self.chosen_states.clear();
        // The clip being looked at stays, keys and all (`collect_clips`
        // knows it again in its new file).
        self.collect_clips();
        self.relayout();
        self.reset_tree();
    }

    fn editable(&self) -> bool {
        self.install.game().profile().phx_untyped_editable || self.asset.typed()
    }

    /// Refuses an edit of a file whose schema version the editor does not
    /// know.
    fn ensure_editable(&self) -> Result<()> {
        anyhow::ensure!(
            self.editable(),
            "This asset uses an unknown schema version and is read-only"
        );
        Ok(())
    }

    /// Records the edited file as it is before an edit: one undo step.
    fn checkpoint(&mut self) {
        self.revision += 1;
        self.history.checkpoint(&self.asset);
        self.journal.push(Step {
            scope: self.scope,
            selected: self.selected_id(),
        });
        self.redo_journal.clear();
        // As deep as a file's own history keeps.
        if self.journal.len() > 4096 {
            self.journal.remove(0);
        }
    }

    /// The selected widget of the file being edited, by its object id.
    fn selected_id(&self) -> Option<phx::ObjectId> {
        self.canvas
            .selected
            .and_then(|n| self.nodes.get(n))
            .filter(|n| !n.foreign)
            .and_then(|n| self.asset.widgets().get(n.widget).map(|w| w.object))
            .map(|o| hierarchy::object_id(&self.asset, o))
            .filter(|&id| id != phx::ObjectId::None)
    }

    /// Undoes (or redoes) the last step of any file: the screen or a
    /// template edited in place, which is entered again if need be. The
    /// selection goes back to the widget selected when the step was made
    /// (the one a delete removed), else stays where it was.
    pub fn undo(&mut self, redo: bool) -> bool {
        // Half-typed values go, in every field: undo is about committed
        // steps (round 11 kept a Preview value open across an undo, which
        // could then land on another file's input).
        self.editing = None;
        self.layout_edit = None;
        self.layout_target = None;
        self.preview.edit = None;
        if let Some(b) = &mut self.binder {
            b.edit = None;
        }
        // Its span may name something else after a structural step.
        self.graph.pick = None;
        let scope = self.scope;
        let kept = self.selected_id();
        loop {
            let step = if redo {
                self.redo_journal.pop()
            } else {
                self.journal.pop()
            };
            let Some(step) = step else {
                return false;
            };
            let at = step.scope;
            if at != self.scope {
                match at {
                    Some((g, key)) => {
                        if self.enter_template(g, key).is_err() {
                            continue;
                        }
                    }
                    None => self.leave_template(),
                }
            }
            let done = if redo {
                self.history.redo(&mut self.asset)
            } else {
                self.history.undo(&mut self.asset)
            };
            if !done {
                continue;
            }
            self.revision += 1;
            // Selected keys are indices into the clip as it was: after a
            // step they would name keys nobody picked (round 6: an undone
            // add moved the selection onto the end key, and Delete took
            // that). Nothing stays selected.
            self.player.key = None;
            self.player.also.clear();
            // Going the other way selects what is selected now.
            let back = Step {
                scope: at,
                selected: kept.filter(|_| scope == at),
            };
            if redo {
                self.journal.push(back);
            } else {
                self.redo_journal.push(back);
            }
            self.canvas.selected = None;
            self.selected = None;
            // The step may have been structural.
            self.images.forget(self.asset.guid());
            self.collect_clips();
            self.relayout();
            self.reset_tree();
            if let Some(id) = step.selected.or(kept.filter(|_| self.scope == scope)) {
                let widgets = self.asset.widgets();
                if let Some(n) = hierarchy::find_widget(&self.asset, &widgets, id)
                    .and_then(|w| self.nodes.iter().position(|n| !n.foreign && n.widget == w))
                {
                    self.select_widget(n);
                }
            }
            let file = match at {
                None => "the screen".to_owned(),
                Some((g, _)) => format!("the {} template", self.template_name(g)),
            };
            self.message = Some((
                format!(
                    "{} a change to {file}",
                    if redo { "Redid" } else { "Undid" }
                ),
                false,
            ));
            return true;
        }
    }

    /// Stages every file with unsaved edits into the mod: the file being
    /// edited, the screen around a template edited in place, and templates
    /// edited in place and left. Only what changed: an untouched copy of a
    /// shipped file would only hide later game patches of it. Returns how
    /// many files were staged.
    pub fn stage(&mut self, project: &mut Project) -> Result<usize> {
        anyhow::ensure!(
            project.game() == self.install.game(),
            "UI asset and project belong to different games"
        );
        self.ensure_editable()?;
        let mut staged = 0;
        if self.history.dirty() {
            project.stage(&self.path, self.asset.encode());
            self.history.mark_saved();
            staged += 1;
        }
        if let Some(r) = &mut self.root
            && r.history.dirty()
        {
            project.stage(&r.path, r.asset.encode());
            r.history.mark_saved();
            staged += 1;
        }
        for (g, (asset, history)) in &mut self.edited {
            if history.dirty() {
                project.stage(&format!("{GENERATED}{g}"), asset.encode());
                history.mark_saved();
                staged += 1;
            }
        }
        Ok(staged)
    }

    fn name(&self, crc: u32) -> String {
        self.asset
            .name(names(), crc)
            .map_or_else(|| format!("{crc:08x}"), str::to_owned)
    }

    fn rows(&self) -> Vec<tree::Row<Row>> {
        let leaf = |node, depth| tree::Row {
            node,
            depth,
            disclosure: Disclosure::Leaf,
        };
        let mut rows = Vec::new();
        let key = |r: &Row| match r {
            Row::Node(i) => Some(*i),
            _ => None,
        };
        let records = |r: &Row| {
            let Row::Node(i) = *r else { return Vec::new() };
            self.asset.nodes()[i]
                .records
                .clone()
                .filter(|&s| self.show_opaque || self.asset.spans()[s].record != Record::Opaque)
                .map(Row::Record)
                .collect()
        };
        let nodes = (0..self.asset.nodes().len()).map(Row::Node);
        self.tree.flatten_into(&mut rows, 0, nodes, &key, &records);
        rows.extend((0..self.asset.properties().len()).map(|p| leaf(Row::Property(p), 0)));
        rows
    }

    /// One-line description of a span for the tree and inspector.
    fn describe(&self, span: usize) -> String {
        let s = &self.asset.spans()[span];
        match &s.record {
            Record::Header => format!("header PHX{}", self.asset.header().kind_str()),
            Record::Types => format!("class table ({})", self.asset.types().len()),
            Record::Layout => format!("layout ({} tokens)", self.asset.layout().tokens.len()),
            Record::Object(o) => format!("{} · {} props", self.name(o.class), o.prop_count),
            Record::Str(r) => format!("\"{}\"", r.text),
            Record::Ref(r) => format!("→ {} {}", self.name(r.class), r.id),
            Record::Color([r, g, b]) => format!("color #{r:02x}{g:02x}{b:02x}"),
            Record::Value(v) => format!("{v:?}"),
            Record::Null => "null".into(),
            Record::Count(n) => format!("{n} elements"),
            Record::Default(v) => format!("default {}", default_text(v)),
            Record::Bytes => format!("{} bytes", s.range.len()),
            Record::Opaque => format!("opaque {} B", s.range.len()),
        }
    }

    /// Text shown when editing starts, or `None` when the span is read-only:
    /// an enum entry by its name where its enum is known (`span_enum`).
    fn edit_text(&self, span: usize) -> Option<String> {
        if !self.editable() {
            return None;
        }
        if let Record::Default(DefaultValue::U32(id)) = self.asset.spans().get(span)?.record
            && let Some(name) = self
                .span_enum(span)
                .and_then(|g| phx::enum_entry_name(g, id))
        {
            return Some(name);
        }
        typed_text(&self.asset, span)
    }

    /// The enum whose entry the value at `span` is, when that is known: a
    /// template input's own, or a fixed value's from its binding: the other
    /// side of an Equal, else what it is copied or passed into (round 15:
    /// Equal's Horizontal refused "Vertical" typed over it).
    fn span_enum(&self, span: usize) -> Option<phx::Guid> {
        if let Some(p) = self.asset.properties().iter().find(|p| p.default == span) {
            return p.enum_asset.map(|r| r.id);
        }
        // The binding table the Bindings pane last built, when it is this
        // file's (the edited template's while one is edited in place).
        let (file, _, ft) = self.table_cache.as_ref()?;
        if file.0 != self.scope.map(|s| s.0) {
            return None;
        }
        let (&slot, _) = ft.table.constants.iter().find(|&(_, &sp)| sp == span)?;
        let (i, b) = ft
            .table
            .bindings
            .iter()
            .enumerate()
            .find(|(_, b)| b.sources.contains(&slot))?;
        let k = b.sources.iter().position(|&s| s == slot)?;
        match ft.converter(i).as_str() {
            "Equal" | "NotEqual" => ft.enum_of(*b.sources.get(1 - k.min(1))?),
            _ => ft.enum_of(*b.targets.first()?),
        }
    }

    /// Puts `rgb` into colour constant `span` (its ColorType's own colour,
    /// or a three-number vector): an undo step of its own when `step`, else
    /// part of the one before (a picker's drags are one step).
    pub(super) fn set_colour(&mut self, span: usize, rgb: [u8; 3], step: bool) -> Result<()> {
        self.ensure_editable()?;
        let edit = match self.asset.spans().get(span).map(|s| &s.record) {
            Some(Record::Default(DefaultValue::Object(_))) => Edit::Color {
                span: constant_colour(&self.asset, span)
                    .ok_or_else(|| anyhow::anyhow!("This colour is a palette entry's"))?,
                rgb,
            },
            Some(Record::Default(DefaultValue::Vec(v))) if v.len() == 3 => Edit::Default {
                span,
                value: DefaultValue::Vec(rgb.map(|c| f32::from(c).to_bits()).to_vec()),
            },
            _ => bail!("Not a colour"),
        };
        let edited = self.asset.apply(&edit)?;
        // After a save the file is clean: a pick then is a new step, or
        // Save would see nothing to write and Quit nothing to ask about
        // (round 14: a pick after Ctrl S, picker still open, was lost).
        if step || !self.history.dirty() {
            self.checkpoint();
        } else {
            self.revision += 1;
        }
        self.asset = edited.asset;
        self.collect_clips();
        self.relayout();
        self.message = Some(("Edited".into(), false));
        Ok(())
    }

    /// The PropertyType the binding constant whose value is span `span`
    /// is stored as.
    fn constant_kind(&self, span: usize) -> Option<u32> {
        let table = self.asset.bindings(&self.asset.widgets());
        let (&slot, _) = table.constants.iter().find(|&(_, &sp)| sp == span)?;
        match table.slots.get(slot)? {
            phx::binding::Slot::Constant { kind, .. } => *kind,
            _ => None,
        }
    }

    fn commit(&mut self, span: usize, text: &str) -> Result<()> {
        self.ensure_editable()?;
        let edit = match &self.asset.spans()[span].record {
            Record::Str(_) => Edit::Text {
                span,
                text: text.to_owned(),
            },
            Record::Color(_) => Edit::Color {
                span,
                rgb: parse_rgb(text)?,
            },
            // A colour constant: its own colour (`constant_colour`).
            Record::Default(DefaultValue::Object(_)) => Edit::Color {
                span: constant_colour(&self.asset, span)
                    .ok_or_else(|| anyhow::anyhow!("This colour is a palette entry's"))?,
                rgb: parse_rgb(text)?,
            },
            // Three numbers stored as a Color (written by an earlier
            // build): channels, 0 to 255, as any colour (round 15: a later
            // edit took [255, 300, -1] and saved it).
            Record::Default(DefaultValue::Vec(v))
                if v.len() == 3 && self.constant_kind(span) == Some(bindings::T_COLOR) =>
            {
                Edit::Default {
                    span,
                    value: DefaultValue::Vec(
                        parse_rgb(text)?.map(|c| f32::from(c).to_bits()).to_vec(),
                    ),
                }
            }
            // An enum entry by its name, any case, where its enum is known.
            Record::Default(DefaultValue::U32(_))
                if let Some(e) = self.span_enum(span).and_then(phx::enum_info)
                    && text.trim().parse::<u32>().is_err() =>
            {
                let t = text.trim();
                let id = e
                    .entries
                    .iter()
                    .find(|x| x.name.eq_ignore_ascii_case(t))
                    .map(|x| x.id)
                    .ok_or_else(|| {
                        let names: Vec<&str> = e.entries.iter().map(|x| x.name.as_str()).collect();
                        anyhow::anyhow!("{} takes {}", e.name, names.join(", "))
                    })?;
                Edit::Default {
                    span,
                    value: DefaultValue::U32(id),
                }
            }
            Record::Default(old) => Edit::Default {
                span,
                value: parse_default(old, text)?,
            },
            Record::Value(old) => Edit::Value {
                span,
                value: parse_value(old, text)?,
            },
            _ => bail!("This record is read-only"),
        };
        let edited = self.asset.apply(&edit)?;
        self.checkpoint();
        self.asset = edited.asset;
        // Key times and values live in clips the player keeps a copy of.
        self.collect_clips();
        self.relayout();
        if let Some(crc) = edited.old_crc.filter(|_| !edited.crc_refs.is_empty()) {
            self.message = Some((
                format!(
                    "Renamed; the old name crc {crc:08x} is still used at {} place(s) in this file — bindings may break",
                    edited.crc_refs.len()
                ),
                true,
            ));
        } else {
            self.message = Some(("Edited".into(), false));
        }
        Ok(())
    }

    pub fn key_down(&mut self, event: &KeyDownEvent) -> bool {
        // The editor's own shortcuts pass every field: undo, redo, save and
        // the workspace keys work while typing (round 10: Preview values and
        // the Game data filter swallowed them). No field has its own undo.
        if editor_shortcut(&event.keystroke) {
            return false;
        }
        // An open colour picker closes first, putting its colour back,
        // whichever pane has the keys (the List's swatches open it with the
        // canvas's).
        if event.keystroke.key == "escape" && self.cancel_pick() {
            return true;
        }
        if self.layout_key(event) || self.preview_key(event) || self.binder_key(event) {
            return true;
        }
        // While the widget tree is narrowed, Esc first brings it back; the
        // next one selects the parent.
        if event.keystroke.key == "escape" && self.editing.is_none() && !self.tree_filter.is_empty()
        {
            self.tree_filter.clear();
            return true;
        }
        let Some(edit) = &mut self.editing else {
            return self.keys == Keys::Canvas && self.canvas_key(event);
        };
        // Tab finishes a value as Enter does, refusal and all (layout fields
        // did; pins, List chips and labels ignored it, round 14). The
        // searches keep Tab out of it.
        if event.keystroke.key == "tab"
            && edit.field != timeline::CLIP_FILTER
            && edit.field != hierarchy::TREE_FILTER
        {
            if let Err(e) = self.commit_editing() {
                self.message = Some((format!("Not applied: {e:#}"), true));
            }
            return true;
        }
        match edit.key_down(event) {
            // The timeline's clip filter narrows as it is typed.
            EditEvent::Commit | EditEvent::Cancel if edit.field == timeline::CLIP_FILTER => {
                self.editing = None;
            }
            EditEvent::Changed if edit.field == timeline::CLIP_FILTER => {
                self.player.filter = edit.text.clone();
            }
            // The widget tree's search narrows as it is typed; Enter
            // selects the first match, Esc clears the search.
            EditEvent::Changed if edit.field == hierarchy::TREE_FILTER => {
                self.tree_filter = edit.text.clone();
            }
            EditEvent::Commit if edit.field == hierarchy::TREE_FILTER => {
                self.editing = None;
                if let Some(&first) = self.tree_matches().first() {
                    self.select_widget(first);
                    self.frame_rect(self.extent(first));
                }
            }
            EditEvent::Cancel if edit.field == hierarchy::TREE_FILTER => {
                self.editing = None;
                self.tree_filter.clear();
            }
            // Refused, the field keeps the text to fix (round 13: "abc"
            // was gone, and the status spoke Rust).
            EditEvent::Commit => {
                if let Err(e) = self.commit_editing() {
                    self.message = Some((format!("Not applied: {e:#}"), true));
                }
            }
            EditEvent::Cancel => self.editing = None,
            EditEvent::Changed => {}
            // Ctrl 1, Ctrl S and the like stay the editor's shortcuts.
            // Plain keys (arrows) stay here: they would nudge the widget
            // being edited.
            EditEvent::Ignored if event.keystroke.modifiers.control => return false,
            EditEvent::Ignored => {}
        }
        true
    }
}

impl UiEditor {
    /// Arrows nudge the selected widget a design pixel (Shift: 10), F
    /// frames it, Home fits the whole page.
    fn canvas_key(&mut self, event: &KeyDownEvent) -> bool {
        let m = event.keystroke.modifiers;
        if m.control || m.platform || m.alt {
            return false;
        }
        let step = if m.shift { 10. } else { 1. };
        let d = match event.keystroke.key.as_str() {
            "left" => [-step, 0.],
            "right" => [step, 0.],
            "up" => [0., -step],
            "down" => [0., step],
            "f" => {
                // The selected widget, or with the root or nothing selected,
                // whatever is visible (small templates sit in a corner of
                // their 1920×1080 page).
                // What draws: containers and page-sized backdrops say
                // nothing about where the artwork is. Editing a template
                // in place, only its part.
                let [pw, ph] = self.canvas.view.size;
                let r = match self.canvas.selected.filter(|&n| n > 0) {
                    Some(n) => Some(self.extent(n)),
                    None => self
                        .nodes
                        .iter()
                        .enumerate()
                        .skip(1)
                        .filter(|(i, n)| {
                            n.visible
                                && n.rect[2] > 0.
                                && n.rect[3] > 0.
                                && n.kind != canvas::Kind::Container
                                && n.rect[2] * n.rect[3] < 0.7 * pw * ph
                                && self.scoped(*i)
                        })
                        .map(|(_, n)| n.rect)
                        .reduce(|a, b| {
                            let x0 = a[0].min(b[0]);
                            let y0 = a[1].min(b[1]);
                            let x1 = (a[0] + a[2]).max(b[0] + b[2]);
                            let y1 = (a[1] + a[3]).max(b[1] + b[3]);
                            [x0, y0, x1 - x0, y1 - y0]
                        }),
                };
                if let Some(r) = r {
                    self.frame_rect(r);
                }
                return true;
            }
            "home" => {
                self.canvas.view.region = None;
                self.canvas.view.refit();
                self.canvas.view.fit(self.canvas_bounds.size);
                return true;
            }
            "escape" => return self.escape(),
            _ => return false,
        };
        // As a drag does: a template's widget moves the container instance
        // holding it.
        let Some(node) = self
            .canvas
            .selected
            .filter(|&n| n > 0)
            .and_then(|n| self.movable(n))
        else {
            return false;
        };
        let before = self.nodes[node].rect;
        let after = [before[0] + d[0], before[1] + d[1], before[2], before[3]];
        self.canvas_change(canvas::Change {
            widget: self.nodes[node].widget,
            before,
            after,
        });
        true
    }

    /// What node `n` covers on the page: its box with every drawn widget
    /// in it. Groups are often 0×0 holders whose box frames nothing; with
    /// nothing in it drawn (a lock icon while unlocked), where its hidden
    /// parts would be.
    fn extent(&self, n: usize) -> [f32; 4] {
        let depth = self.nodes[n].depth;
        let inside = || {
            self.nodes[n + 1..]
                .iter()
                .take_while(move |c| c.depth > depth)
                .filter(|c| c.kind != canvas::Kind::Container)
        };
        let union = |nodes: &mut dyn Iterator<Item = &canvas::Node>| {
            nodes
                .map(|c| c.rect)
                .filter(|r| r[2] > 0. && r[3] > 0.)
                .reduce(|a, b| {
                    let (x0, y0) = (a[0].min(b[0]), a[1].min(b[1]));
                    let (x1, y1) = (
                        (a[0] + a[2]).max(b[0] + b[2]),
                        (a[1] + a[3]).max(b[1] + b[3]),
                    );
                    [x0, y0, x1 - x0, y1 - y0]
                })
        };
        let me = std::iter::once(&self.nodes[n]);
        union(&mut me.clone().chain(inside().filter(|c| c.visible)))
            .or_else(|| union(&mut me.chain(inside())))
            .unwrap_or(self.nodes[n].rect)
    }

    /// Zooms and pans so `r` (design pixels) fills most of the canvas.
    fn frame_rect(&mut self, r: [f32; 4]) {
        let (w, h) = (
            f32::from(self.canvas_bounds.size.width),
            f32::from(self.canvas_bounds.size.height),
        );
        if w < 10. || h < 10. {
            return;
        }
        // At most 4×: a widget a few pixels big (a hidden scroll bar)
        // framed at 16× showed blurred emptiness.
        self.canvas.view.frame(r, w, h, 120., 4.);
    }
}

/// `texts_english.xml`, the language the editor shows labels in.
// TODO(alex): follow the game's language setting.
fn load_texts(install: &Install) -> std::collections::HashMap<i64, String> {
    install
        .read(install.game().profile().texts_path)
        .map(|b| phx::parse_texts(&String::from_utf8_lossy(&b)))
        .unwrap_or_default()
}

/// Editable text for a scalar value; `None` for read-only kinds.
fn value_text(v: &Value) -> Option<String> {
    let join = |xs: &[f32]| {
        xs.iter()
            .map(|x| format!("{x}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Some(match v {
        Value::Bool(b) => b.to_string(),
        Value::U8(x) => x.to_string(),
        Value::U16(x) => x.to_string(),
        Value::S32(x) => x.to_string(),
        Value::U32(x) => x.to_string(),
        Value::U64(x) => x.to_string(),
        Value::F32(x) => x.to_string(),
        Value::Vec2(x) => join(x),
        Value::Vec3(x) => join(x),
        Value::Vec4(x) => join(x),
        // By the entry's name where the game ships one (a label's
        // HorizontalOverflow 4 is Ellipsis).
        Value::Enum { ty, value } => anno_formats::phx::enum_of_type(*ty)
            .and_then(|e| e.entries.into_iter().find(|x| x.id == *value))
            .map_or_else(|| value.to_string(), |x| x.name),
        _ => return None,
    })
}

type NodeKey = Vec<(usize, bool, u32)>;

/// Who canvas node `n` is across layouts: its widget and its parents' up
/// to the page, each with whether it is another file's and which sample
/// list item it is. A flag or a list showing more widgets renumbers the
/// nodes; this stays.
fn node_key(nodes: &[canvas::Node], parents: &[Option<usize>], n: usize) -> NodeKey {
    let mut key = Vec::new();
    let mut at = Some(n);
    while let Some(i) = at {
        key.push((nodes[i].widget, nodes[i].foreign, nodes[i].item));
        at = parents.get(i).copied().flatten();
    }
    key
}

/// Gives freed heap back to the system. glibc keeps what a burst of
/// layouts freed (a deep template's relayouts, the flag probe's four
/// trees) in its per-thread arenas, and RSS stays at the peak: on the
/// construction menu, nine templates deep and back four times, idle RSS
/// went from 3.2 GB to 0.9 GB and the peak from 3.4 to 1.6 GB (round 12
/// reviewers saw 3.5–4.3 GB). Capping glibc's arenas went lower still but
/// slowed the parallel layouts by 12–30%; mimalloc kept 1.8 GB. A trim costs 2–4 ms
/// there, so at most every 2 s unless `now` (playback relayouts every
/// frame). Whether it trimmed.
pub(super) fn release_heap(now: bool) -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
        let Ok(mut last) = LAST.lock() else {
            return false;
        };
        if !now && last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2)) {
            return false;
        }
        *last = Some(std::time::Instant::now());
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: glibc's own call, safe from any thread.
        unsafe { malloc_trim(0) };
        true
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = now;
        false
    }
}

/// Span `span`'s value as a field shows it for typing, when it can be typed
/// over: text, a colour, a scalar, a default (scalar, vector as
/// `[255, 255, 255]`, text bare), or a colour constant's own colour. The
/// graph offers a fixed value on a pin as a field only when this has one.
// `span` is `asset`'s: a span of another (a template's clip key) must not
// index it, or panic past its end.
pub(super) fn typed_text(asset: &Asset, span: usize) -> Option<String> {
    let rgb = |[r, g, b]: [u8; 3]| format!("{r:02x}{g:02x}{b:02x}");
    match &asset.spans().get(span)?.record {
        Record::Str(r) => Some(r.text.clone()),
        Record::Color(c) => Some(rgb(*c)),
        Record::Value(v) => value_text(v),
        Record::Default(
            v @ (DefaultValue::Bool(_)
            | DefaultValue::S32(_)
            | DefaultValue::U32(_)
            | DefaultValue::F32(_)),
        ) => Some(default_text(v)),
        Record::Default(DefaultValue::Vec(v)) => Some(format!(
            "[{}]",
            v.iter()
                .map(|b| f32::from_bits(*b).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Record::Default(DefaultValue::Str(s)) => Some(s.clone()),
        Record::Default(DefaultValue::Object(_)) => {
            let Record::Color([r, g, b]) = asset.spans()[constant_colour(asset, span)?].record
            else {
                return None;
            };
            Some(format!("[{r}, {g}, {b}]"))
        }
        _ => None,
    }
}

/// The colour fixed value `span` is, if it is one: a ColorType's own colour
/// (not a palette entry), or three numbers 0 to 255 stored as a Color.
pub(super) fn constant_rgb(asset: &Asset, span: usize, kind: Option<u32>) -> Option<[u8; 3]> {
    match &asset.spans().get(span)?.record {
        Record::Default(DefaultValue::Object(_)) => {
            match asset.spans()[constant_colour(asset, span)?].record {
                Record::Color(c) => Some(c),
                _ => None,
            }
        }
        Record::Default(DefaultValue::Vec(v))
            if v.len() == 3 && kind == Some(bindings::T_COLOR) =>
        {
            let c: Vec<u8> = v
                .iter()
                .map(|b| f32::from_bits(*b))
                .map(|x| (0.0..=255.0).contains(&x).then(|| x.round() as u8))
                .collect::<Option<_>>()?;
            Some([c[0], c[1], c[2]])
        }
        _ => None,
    }
}

/// A colour constant (`Default(Object(ColorType))`, the ColorType embedded
/// after it) when its colour is its own: the span of its Color record. One
/// that names a palette entry shows the palette's colour, and typing over
/// its own would change nothing on screen; None then.
// TODO(alex): offer the palette's entries for those.
pub(super) fn constant_colour(asset: &Asset, span: usize) -> Option<usize> {
    let spans = asset.spans();
    let Record::Default(DefaultValue::Object(_)) = spans.get(span)?.record else {
        return None;
    };
    let object = span + 1;
    for (k, s) in spans.iter().enumerate().skip(object + 1) {
        let Some(f) = s.field else { continue };
        if f.object < object {
            break;
        }
        match s.record {
            // The ColorType's palette entry: 0 is none.
            Record::Value(Value::U32(entry)) if f.object == object && entry != 0 => return None,
            Record::Color(_) => return Some(k),
            _ => {}
        }
    }
    None
}

/// A colour typed as six hex digits (`ff8800`, `#ff8800`) or three numbers
/// 0 to 255 (`[255, 136, 0]`, as the graph shows a colour).
pub(super) fn parse_rgb(text: &str) -> Result<[u8; 3]> {
    let t = text.trim();
    let hex = t.trim_start_matches('#');
    if hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    // Every number counts: one out of range refuses the colour (round 13:
    // [255, 300, 0, 0] was stored as [255, 0, 0], silently).
    let parts = numbers(t)
        .map_err(|e| anyhow::anyhow!("{e}: colors are RRGGBB or three numbers 0 to 255"))?;
    let [r, g, b] = parts[..] else {
        bail!("Colors are RRGGBB or three numbers 0 to 255");
    };
    let byte = |v: f32| {
        anyhow::ensure!((0.0..=255.0).contains(&v), "{v} is not 0 to 255");
        Ok(v.round() as u8)
    };
    Ok([byte(r)?, byte(g)?, byte(b)?])
}

/// Numbers typed as `[255, 0, 0]`, `255 0 0` or `(255, 0, 0)`: each one a
/// finite number, or the whole text is refused.
fn numbers(text: &str) -> Result<Vec<f32>> {
    text.trim()
        .trim_matches(|c| matches!(c, '[' | ']' | '(' | ')'))
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| anyhow::anyhow!("{p} is not a number"))
        })
        .collect()
}

/// `v` when it is a real number: infinities and NaN written into a file
/// break the game's layout, and nothing shipped stores one.
fn finite(v: f32) -> Result<f32> {
    anyhow::ensure!(v.is_finite(), "Enter a finite number");
    Ok(v)
}

/// `text` as a default of `old`'s kind, a vector as many numbers as it
/// holds (`[24, 24]` as the graph shows one): the one grammar of the
/// graph's fixed values, the List and Preview data (round 14: Preview
/// took "inf" and blanked the canvas). What it refuses it says in plain
/// words, not the parser's ("invalid float literal", round 13).
pub(super) fn parse_default(old: &DefaultValue, text: &str) -> Result<DefaultValue> {
    let t = text.trim();
    Ok(match old {
        DefaultValue::Bool(_) => DefaultValue::Bool(match t {
            "1" | "true" | "yes" => 1,
            "0" | "false" | "no" => 0,
            _ => bail!("Booleans are true/false or 1/0"),
        }),
        DefaultValue::S32(_) => DefaultValue::S32(
            t.parse()
                .map_err(|_| anyhow::anyhow!("Enter a whole number"))?,
        ),
        DefaultValue::U32(_) => DefaultValue::U32(
            t.parse()
                .map_err(|_| anyhow::anyhow!("Enter a whole number, 0 or more"))?,
        ),
        DefaultValue::F32(_) => DefaultValue::F32(
            finite(t.parse().map_err(|_| anyhow::anyhow!("Enter a number"))?)?.to_bits(),
        ),
        DefaultValue::Vec(v) => {
            let parts = numbers(t)?;
            anyhow::ensure!(parts.len() == v.len(), "Enter {} numbers", v.len());
            DefaultValue::Vec(parts.iter().map(|x| x.to_bits()).collect())
        }
        DefaultValue::Str(_) => DefaultValue::Str(text.to_owned()),
        _ => bail!("This default is read-only"),
    })
}

/// Parses `text` as a value of the same kind as `old`.
fn parse_value(old: &Value, text: &str) -> Result<Value> {
    let t = text.trim();
    let floats = |n: usize| -> Result<Vec<f32>> {
        let xs: Vec<f32> = t
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(|s| Ok(finite(s.parse()?)?))
            .collect::<Result<_>>()?;
        anyhow::ensure!(xs.len() == n, "Enter {n} numbers separated by commas");
        Ok(xs)
    };
    Ok(match old {
        Value::Bool(_) => Value::Bool(match t {
            "1" | "true" | "yes" => true,
            "0" | "false" | "no" => false,
            _ => bail!("Booleans are true/false or 1/0"),
        }),
        Value::U8(_) => Value::U8(t.parse()?),
        Value::U16(_) => Value::U16(t.parse()?),
        Value::S32(_) => Value::S32(t.parse()?),
        Value::U32(_) => Value::U32(t.parse()?),
        Value::U64(_) => Value::U64(t.parse()?),
        Value::F32(_) => Value::F32(finite(t.parse()?)?),
        Value::Vec2(_) => Value::Vec2(floats(2)?.try_into().expect("two")),
        Value::Vec3(_) => Value::Vec3(floats(3)?.try_into().expect("three")),
        Value::Vec4(_) => Value::Vec4(floats(4)?.try_into().expect("four")),
        // An entry by its name too, any case; the names in the refusal.
        Value::Enum { ty, .. } => Value::Enum {
            ty: *ty,
            value: match (t.parse(), anno_formats::phx::enum_of_type(*ty)) {
                (Ok(v), _) => v,
                (Err(_), Some(e)) => e
                    .entries
                    .iter()
                    .find(|x| x.name.eq_ignore_ascii_case(t))
                    .map(|x| x.id)
                    .ok_or_else(|| {
                        let names: Vec<&str> = e.entries.iter().map(|x| x.name.as_str()).collect();
                        anyhow::anyhow!("{} takes {}", e.name, names.join(", "))
                    })?,
                (Err(_), None) => bail!("Enter the entry's number"),
            },
        },
        _ => bail!("This value is read-only"),
    })
}

/// A template property's default as the inspector and tree show it: an
/// enum entry by name, by the property's own enum (round 15: the inspector
/// said 1403909053 where Bindings said Begin).
pub(super) fn property_default_text(asset: &Asset, p: &phx::CustomProperty) -> String {
    let v = asset.property_default(p);
    if let (DefaultValue::U32(id), Some(e)) = (v, p.enum_asset)
        && let Some(name) = phx::enum_entry_name(e.id, *id)
    {
        return name;
    }
    default_text(v)
}

fn default_text(v: &DefaultValue) -> String {
    match v {
        DefaultValue::None => "none".into(),
        DefaultValue::Bool(b) => (*b != 0).to_string(),
        DefaultValue::S32(v) => v.to_string(),
        // A hashed enum entry by its name (see `short`).
        DefaultValue::U32(v) => phx::unique_entry_name(*v).unwrap_or_else(|| v.to_string()),
        DefaultValue::F32(bits) => f32::from_bits(*bits).to_string(),
        DefaultValue::Vec(v) => format!(
            "({})",
            v.iter()
                .map(|b| f32::from_bits(*b).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DefaultValue::Str(s) => format!("\"{s}\""),
        DefaultValue::ObjectId(g) => g.to_string(),
        DefaultValue::Object(class) => format!("object {class:08x}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .map(|c| {
            c.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Editor {
    pub fn open_ui_asset(&mut self, path: &str, cx: &mut Context<Self>) {
        // The preview was of the file being opened, or of one passed over.
        self.ui_peek = None;
        self.ensure_ui_index(cx);
        let Some(install) = self.install.clone() else {
            return self.message("No install is open", true);
        };
        if self.ui.as_ref().is_some_and(|u| u.dirty()) {
            return self.message(
                "Save the open UI asset (Ctrl S) before opening another",
                true,
            );
        }
        let path = path.to_owned();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        names();
                        // Some shipped screens still panic in the layout
                        // (Construction Menu, Options): report it as a failed
                        // open instead of taking the editor down with it.
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            UiEditor::load(install, &path)
                        }))
                        .unwrap_or_else(|panic| {
                            // The panic text is for the log; the status bar
                            // says what it means for the modder.
                            let why = (panic.downcast_ref::<String>().map(String::as_str))
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("unknown error");
                            eprintln!("UI layout of {path} panicked: {why}");
                            Err(anyhow::anyhow!(LAYOUT_UNSUPPORTED))
                        })
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(mut ui) => {
                        // The game view outlives the file it was built around.
                        if let Some(old) = this.ui.take() {
                            ui.game = old.game;
                            let open = ui.path.clone();
                            ui.game.screens.retain(|s| s.path != open);
                        }
                        let a = &ui.asset;
                        this.message(
                            // Named and counted as the file list and the
                            // tree do, so the three agree.
                            format!(
                                "Opened {} · {} · {}{}",
                                this.ui_find.display_name(&path).unwrap_or_else(|| path
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or("")
                                    .to_owned()),
                                crate::project::plural(
                                    ui.nodes
                                        .iter()
                                        .filter(|n| !n.foreign)
                                        .count()
                                        .saturating_sub(1),
                                    "own widget",
                                    "own widgets"
                                ),
                                crate::project::plural(
                                    a.properties().len(),
                                    "property",
                                    "properties"
                                ),
                                if ui.editable() {
                                    ""
                                } else {
                                    " · Read-only: unknown schema version"
                                }
                            ),
                            false,
                        );
                        this.ui = Some(ui);
                        this.ui_failed = None;
                        this.switch(Workspace::Ui);
                    }
                    Err(e) => {
                        let name = this
                            .ui_find
                            .display_name(&path)
                            .unwrap_or_else(|| path.clone());
                        let text = if e.to_string() == LAYOUT_UNSUPPORTED {
                            this.ui_failed = Some(name.clone());
                            format!("{name} couldn’t open: its layout isn’t supported yet")
                        } else {
                            format!("Could not open {name}: {e:#}")
                        };
                        this.message(text, true)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens another asset by guid, or shows an image path in the browser.
    fn follow(&mut self, target: String, cx: &mut Context<Self>) {
        let Some(install) = self.install.clone() else {
            return;
        };
        if install.file(&target).is_some() {
            return self.open_path(&target, cx);
        }
        // Image strings name source PNGs; the game ships them as DDS.
        let dds = target.replace('\\', "/").replace(".png", ".dds");
        if install.file(&dds).is_some() {
            return self.open_path(&dds, cx);
        }
        self.message(format!("{target} is not in the install"), true);
    }

    /// The laid-out widgets: pan with middle/right drag, zoom with the
    /// wheel, click to select, drag to move, handles to resize.
    pub fn render_ui_canvas(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Game fonts loaded by the last layout become usable by family.
        if let Some(ui) = &mut self.ui
            && !ui.images.new_fonts.is_empty()
        {
            let fonts = std::mem::take(&mut ui.images.new_fonts)
                .into_iter()
                .map(|f| std::borrow::Cow::Owned(f.bytes.to_vec()))
                .collect();
            if let Err(e) = cx.text_system().add_fonts(fonts) {
                eprintln!("UI fonts: {e:#}");
            }
        }
        // The empty state the other workspaces use: a heading, a line, and
        // a way in.
        let Some(ui) = &self.ui else {
            use gpui_editor_kit::widgets::{Tone, action, empty_state_icon, hint};
            // A few screens to open in one click; "Find a screen" only
            // repeated the search box over the list.
            // Not the one that just failed: offering it again only repeats
            // the failure.
            let failed = self.ui_failed.as_deref().map(|n| format!("Open {n}"));
            let starters: Vec<_> = (self.starter_screens().into_iter())
                .filter(|(_, title)| Some(title) != failed.as_ref())
                .collect();
            let one = starters.len() == 1;
            let mut row = div().pt_2().flex().flex_wrap().justify_center().gap_2();
            // The first starter primary (Main Title, where the game opens),
            // as every other empty state has one, whatever the pointer is
            // over: the Inspector's Open for a pointed-at file stays
            // secondary, since nothing was chosen. Swapping the filled button
            // as the pointer crossed the list made the same state look two
            // ways. The described file is not offered here too: two
            // same-named buttons 700 px apart.
            for (n, (path, title)) in starters.into_iter().enumerate() {
                let tone = if n == 0 {
                    Tone::Primary
                } else {
                    Tone::Secondary
                };
                row = row.child(action(title, tone).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.open_ui_asset(&path, cx);
                        cx.notify();
                    },
                )));
            }
            // After a failed open the canvas says so in place, not only in
            // the status bar.
            let heading = match &self.ui_failed {
                Some(name) => format!("{name} couldn’t open"),
                None => "No screen open".to_owned(),
            };
            return empty_state_icon(gpui_editor_kit::icon::Icon::LAYOUT, heading)
                .when(self.ui_failed.is_some(), |d| {
                    d.child(hint("Its layout isn’t supported yet."))
                })
                // "UI files" as a link to the pane: the left dock may be on
                // another tab.
                .child(
                    div()
                        .flex()
                        .gap(px(3.))
                        .child(hint("Click a screen in"))
                        // Link and the comma after it touch, as in text.
                        .child(
                            div()
                                .flex()
                                .child(
                                    div()
                                        .id("ui-canvas-files-link")
                                        .text_size(px(11.))
                                        .text_color(theme::get().accent)
                                        .cursor_pointer()
                                        .hover(|s| s.underline())
                                        .child("UI files")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_pane(crate::editor::Pane::UiFiles);
                                            cx.notify();
                                        })),
                                )
                                .child(hint(if one {
                                    " to open it, or start with this one."
                                } else {
                                    " to open it, or start with one of these."
                                })),
                        ),
                )
                // The project by name, tying the canvas to the switcher above.
                .child(hint(format!(
                    "Edits save into ‘{}’.",
                    self.project.info.name
                )))
                .child(row)
                .into_any_element();
        };
        if ui.nodes.is_empty() {
            return empty_pane("This asset has no widget tree (only widget templates lay out)");
        }
        let _span = gpui_editor_kit::profile::span("ui canvas elements");
        let entity = cx.entity();
        let on_handle = {
            let entity = entity.clone();
            move |node: usize,
                  handle: Handle,
                  at: Point<Pixels>,
                  window: &mut Window,
                  cx: &mut App| {
                entity.update(cx, |this, cx| {
                    this.focus.focus(window, cx);
                    if let Some(ui) = &mut this.ui {
                        ui.press(at, Modifiers::default(), 1, Some((node, handle)));
                    }
                    cx.notify();
                })
            }
        };
        let drag = ui.drag_preview();
        let mut children = vec![ui.canvas.view.page()];
        // What the game draws is cut to the screen it draws on.
        let mut drawn = ui.game_view_elements();
        let (content, handles) = canvas::render(
            &ui.canvas,
            &ui.nodes,
            &ui.in_scope,
            drag.as_ref().map(|d| (d.0, d.1)),
            ui.visible_region(),
            &ui.gpu,
            Some(on_handle),
        );
        drawn.extend(content);
        children.push(ui.canvas.view.clip_to_page(drawn));
        children.extend(canvas::chrome(ui, drag.as_ref()));
        children.extend(placement::overlay(ui, entity.clone()));
        children.extend(handles);
        children.extend(self.scope_bar(cx));
        if let Some(menu) = &ui.canvas.menu {
            children.push(self.canvas_menu(menu.clone(), cx));
        }
        let bar = ui.game_view_bar(cx);
        let bounds_entity = entity.clone();
        let hint = format!(
            "{:.0}% · Click: deepest widget · Alt click: outward · Esc: parent · Drag: move (Shift: one axis, Alt: no snap) · Right-click: layers · F: frame",
            ui.canvas.view.zoom * 100.
        );
        div()
            .id("ui-canvas")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(crate::colors::UI_CANVAS))
            // A widget library row lands where it is dropped, in the group
            // drawn there.
            .on_drop(cx.listener(|this, drag: &library::LibraryDrag, window, cx| {
                let Some(ui) = &this.ui else { return };
                let at = ui.design_point(window.mouse_position());
                this.add_library_pick(drag.0, hierarchy::Place::Canvas(at));
                cx.notify();
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let moved = bounds_entity.update(cx, |this, _| {
                            this.ui.as_mut().is_some_and(|ui| {
                                ui.canvas_bounds = bounds;
                                ui.canvas.view.fit(bounds.size)
                            })
                        });
                        // This frame was built at the old zoom; a notify
                        // from inside the draw is dropped, so ask for the
                        // next frame instead.
                        if moved {
                            let entity = bounds_entity.clone();
                            window
                                .on_next_frame(move |_, cx| entity.update(cx, |_, cx| cx.notify()));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(children)
            .child(bar)
            // Record changes what a drag does: say so where the drag is.
            .when_some(
                ui.player
                    .recording
                    .then(|| ui.clip().map(|c| (c.name.clone(), ui.player.time)))
                    .flatten(),
                |d, (clip, t)| {
                    d.child(
                        div()
                            .absolute()
                            .top_2()
                            .right_3()
                            .px_2()
                            .py_1()
                            .rounded(px(3.))
                            .bg(crate::colors::get().recording)
                            .text_size(px(11.))
                            .text_color(crate::colors::get().recording_text)
                            .child(format!(
                                "● Recording {clip}: moves and resizes key Position and Scale at {t:.2} s"
                            )),
                    )
                },
            )
            // The island viewport's dark pill: bare grey text read as a
            // stray caption.
            .child(
                gpui_editor_kit::widgets::hint_pill(&hint)
                    .absolute()
                    .bottom_2()
                    .left_3()
                    .max_w(relative(0.8)),
            )
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered
                    && let Some(ui) = &mut this.ui
                    && ui.canvas.hover.take().is_some()
                {
                    this.notify_pane(crate::editor::Pane::UiCanvas, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    // Keys go where the click went, not to a search typed
                    // into before.
                    this.keys_to(Keys::Canvas);
                    if let Some(ui) = &mut this.ui {
                        if ui.interact {
                            ui.interact_at(Some(e.position), interact::State::Press);
                        } else {
                            ui.press(e.position, e.modifiers, e.click_count, None);
                        }
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.canvas.view.begin_pan(e.position);
                    }
                    this.notify_pane(crate::editor::Pane::UiCanvas, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.canvas.menu = None;
                        ui.canvas.right_press = Some(e.position);
                        ui.canvas.view.begin_pan(e.position);
                    }
                    this.notify_pane(crate::editor::Pane::UiCanvas, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let _span = gpui_editor_kit::profile::span("ui canvas mouse move");
                if let Some(ui) = &mut this.ui {
                    let pressed = e.pressed_button == Some(MouseButton::Left);
                    if ui.interact {
                        ui.cursor = e.position;
                        let state = if pressed {
                            interact::State::Press
                        } else {
                            interact::State::Hover
                        };
                        if ui.interact_at(Some(e.position), state) {
                            cx.notify();
                        }
                        return;
                    }
                    // A drag that ended outside the pane ends here.
                    if !pressed && ui.canvas.press.is_some() {
                        ui.release(e.position);
                        cx.notify();
                    }
                    let mut redraw = ui.pointer_move(e.position, e.modifiers, pressed);
                    if ui.canvas.view.drag_to(e.position) || ui.pivot_drag {
                        redraw = redraw.max(canvas::Redraw::Canvas);
                    }
                    match redraw {
                        canvas::Redraw::Nothing => {}
                        canvas::Redraw::Canvas => {
                            this.notify_pane(crate::editor::Pane::UiCanvas, cx)
                        }
                        canvas::Redraw::All => cx.notify(),
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        if ui.interact {
                            ui.interact_at(Some(e.position), interact::State::Hover);
                        } else if ui.pivot_drag {
                            ui.finish_pivot_drag(e.position);
                        } else {
                            ui.release(e.position);
                        }
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.canvas.view.cancel();
                    }
                    this.notify_pane(crate::editor::Pane::UiCanvas, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.canvas.view.cancel();
                        // A right click without a pan lists what is under it.
                        if let Some(start) = ui.canvas.right_press.take() {
                            let d = e.position - start;
                            if f32::from(d.x).hypot(f32::from(d.y)) < 4. {
                                let p = ui
                                    .canvas
                                    .view
                                    .to_design(e.position - ui.canvas_bounds.origin);
                                let stack = ui.stack_at(p);
                                if !stack.is_empty() {
                                    ui.canvas.menu = Some(canvas::Menu {
                                        at: e.position - ui.canvas_bounds.origin,
                                        stack,
                                    });
                                }
                            }
                        }
                    }
                    cx.notify();
                }),
            )
            .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
                let Some(ui) = &mut this.ui else { return };
                let delta = match e.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                    ScrollDelta::Lines(p) => p.y * 32.,
                };
                let local = e.position - ui.canvas_bounds.origin;
                ui.canvas
                    .view
                    .zoom_at(delta, [f32::from(local.x), f32::from(local.y)]);
                this.notify_pane(crate::editor::Pane::UiCanvas, cx);
            }))
            .into_any_element()
    }

    /// One widget property in the inspector: on/off for booleans, a field
    /// for what edits as text, the value otherwise. `bound` marks values a
    /// data binding overrides, whose stored value does not show.
    fn prop_row(
        &self,
        ui: &UiEditor,
        prop: &phx::PropValue,
        label: String,
        bound: Option<phx::binding::Bound>,
        cx: &mut Context<Self>,
    ) -> Div {
        let span = prop.span;
        let overridden = bound.is_some_and(|b| match label.as_str() {
            "Position" => b.position.is_some(),
            "PivotPoint" => b.pivot.is_some(),
            "RotationAngle" => b.rotation.is_some(),
            "Scale" => b.scale.is_some() || b.size.is_some(),
            "ColorTint" => b.tint.is_some(),
            "AlphaTint" => b.alpha.is_some(),
            "Visible" => b.visible.is_some(),
            _ => false,
        });
        let field: AnyElement = match (&prop.value, ui.edit_text(span)) {
            (Value::Bool(on), Some(_)) => {
                let on = *on;
                button(if on { "on" } else { "off" }, on)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = &mut this.ui
                            && let Err(e) = ui.commit(span, if on { "false" } else { "true" })
                        {
                            ui.message = Some((format!("Not applied: {e:#}"), true));
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            (_, Some(text)) => {
                let editing = ui
                    .editing
                    .as_ref()
                    .filter(|e| e.field == span)
                    .map(|e| e.display());
                text_field(("ui-prop", span), &text, editing, "")
                    .flex_1()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_span_edit(span, Keys::Canvas, Filter::Text, window, cx)
                    }))
                    .into_any_element()
            }
            (v, None) => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.))
                .child(match v {
                    Value::Str(s) => format!("\"{s}\""),
                    Value::Ref(r) => format!("→ {} {}", ui.name(r.class), r.id),
                    Value::Color([r, g, b]) => format!("#{r:02x}{g:02x}{b:02x}"),
                    Value::Guid(g) => g.to_string(),
                    Value::Null => "none".into(),
                    _ => "…".into(),
                })
                .into_any_element(),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(120.))
                    .flex_none()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child(label),
            )
            .when(matches!(prop.value, Value::Color(_)), |d| {
                let Value::Color([r, g, b]) = prop.value else { return d };
                d.child(
                    div()
                        .size(px(14.))
                        .flex_none()
                        .rounded(px(2.))
                        .border_1()
                        .border_color(theme::get().border)
                        .bg(rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))),
                )
            })
            .child(field)
            .when(overridden, |d| {
                d.child(
                    div()
                        .id(("ui-prop-bound", span))
                        .px_1()
                        .rounded(px(2.))
                        .bg(theme::get().selected)
                        .text_size(px(10.))
                        .child("bound")
                        .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                            "Set by a data binding",
                            "",
                            "The binding's value shows in game; this stored value only shows without it. See Bindings.",
                        )),
                )
            })
    }

    /// Ends typing in the UI workspace's fields and searches, applying the
    /// value as a click elsewhere would; the keys stay where they are.
    /// Save, leaving (quit, open, switch game) and the workspace switch go
    /// through here.
    // TODO(alex): the island's fields and filter are not ended here; Ctrl S
    // in the Island workspace saves without a half-typed island value.
    pub(crate) fn end_typing(&mut self) {
        let keys = self.ui.as_ref().map(|u| u.keys).unwrap_or_default();
        self.keys_to(keys);
    }

    /// A click took the keys for `keys`: the UI files search stops typing,
    /// and so does every field of the UI workspace (see
    /// `UiEditor::take_keys`). Pane clicks and field activations call this
    /// first, so typing never lands in a field that was left (round 4:
    /// Filter clips typed into the files search, F after a graph click
    /// into the binder's search).
    pub(crate) fn keys_to(&mut self, keys: Keys) {
        self.ui_find.stop_typing();
        // Any handoff ends the asset browser's search too, one between the
        // browser and the preview included (round 11: typing went on into
        // it from the preview); a press in the field itself never gets
        // here (`press_keys_to`), and a click on it starts it again.
        self.browser.stop_typing();
        if let Some(ui) = &mut self.ui {
            ui.take_keys(keys);
        }
    }

    /// A press in a pane gives it the keys, but one in the field being
    /// typed only places the caret there (round 9: it applied the half-typed
    /// value, an undo step, and refused a lone "-").
    pub(crate) fn press_keys_to(&mut self, at: Point<Pixels>, keys: Keys) {
        let typing = self.ui_find.typing().is_some()
            || self.browser.typing().is_some()
            || self.ui.as_mut().is_some_and(|u| u.typing().is_some());
        if typing && gpui_editor_kit::field::in_active_field(at) {
            return;
        }
        self.keys_to(keys);
    }

    /// Starts typing into span `span`'s field, the keys going to `keys`'s
    /// pane. A click on the field already being typed leaves the edit as it
    /// is: the field hands the press to it, which places the caret.
    pub(super) fn start_span_edit(
        &mut self,
        span: usize,
        keys: Keys,
        filter: Filter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .ui
            .as_ref()
            .is_some_and(|ui| ui.editing.as_ref().is_some_and(|e| e.field == span))
        {
            return;
        }
        self.focus.focus(window, cx);
        self.keys_to(keys);
        if let Some(ui) = &mut self.ui
            && let Some(text) = ui.edit_text(span)
        {
            ui.editing = Some(TextEdit::new(span, text, filter));
        }
        cx.notify();
    }

    pub fn render_ui_inspector(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_flag_probe(cx);
        if let Some(ui) = &mut self.ui
            && ui.names.is_empty()
            && let Some(names) = self.ui_find.names()
        {
            ui.names = std::sync::Arc::new(names);
        }
        let mut data = self.data_section(window, cx);
        // What feeds each bound layout field, for its "follows" link.
        let follows = self
            .ui
            .as_mut()
            .and_then(|ui| Some(ui.follows(ui.canvas.selected?)))
            .unwrap_or_default();
        let Some(ui) = &self.ui else {
            return self.ui_peek_panel(cx).unwrap_or_else(|| {
                crate::editor::pane_note("Point at a file in UI files to see what it is here.")
            });
        };
        let a = &ui.asset;
        // What the screen shows depends on these. With a widget selected
        // they sit, folded, under its Data and Evaluated blocks: below its
        // twenty-odd properties nobody found them. With nothing selected,
        // after the Unsaved list and before the file's internals (template
        // properties, references), which a big template has hundreds of.
        let selected = ui.canvas.selected.is_some();
        let mut screen_wide = [
            self.preview_section(window, cx),
            self.game_data_section(selected, window, cx),
        ];
        let mut body = div()
            .id("ui-inspector")
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .w_full();
        if let Some(d) = self.game_view_section(cx) {
            body = body.child(d);
        }
        if ui.canvas.selected.is_none() {
            body = body.child(
                div()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("Select a widget on the canvas or in the Widget tree to edit it."),
            );
            // What Save writes into the mod, and where each edit was made.
            let unsaved = ui.unsaved();
            if !unsaved.is_empty() {
                body = body.child(section(&format!(
                    "Unsaved · {}",
                    crate::project::plural(unsaved.len(), "file", "files")
                )));
            }
            for (file, copies, at) in unsaved {
                let current = ui.scope.map(|s| s.0) == file;
                let (name, detail) = match file {
                    None => (
                        self.ui_find
                            .display_name(&ui.root.as_ref().map_or(&ui.path, |r| &r.path).clone())
                            .unwrap_or_else(|| "This screen".into()),
                        "screen".to_owned(),
                    ),
                    Some(g) => {
                        let id = g.to_string();
                        let name = self
                            .ui_find
                            .index
                            .as_ref()
                            .and_then(|i| i.entries.iter().find(|e| e.file == id))
                            .map_or_else(|| id[..8].to_owned(), |e| e.name.clone());
                        // Every copy changes: say how many this screen has.
                        let detail = match copies {
                            1 => "template".to_owned(),
                            n => format!("template · {n} copies here change"),
                        };
                        (name, detail)
                    }
                };
                let go = match (file, current) {
                    (_, true) => None,
                    (None, false) => Some(("Back to screen", None)),
                    (Some(g), false) => at.map(|k| ("Edit here", Some((g, k)))),
                };
                body = body.child(
                    // Its own id: several "Edit here" buttons would share
                    // their label's.
                    div()
                        .id(SharedString::from(format!("ui-unsaved-{file:?}")))
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().truncate().text_size(px(11.)).child(name))
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(theme::get().muted)
                                        .child(if current {
                                            format!("{detail} · editing")
                                        } else {
                                            detail
                                        }),
                                ),
                        )
                        .when_some(go, |d, (label, target)| {
                            d.child(button(label, false).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(ui) = &mut this.ui {
                                        let r = match target {
                                            Some((g, k)) => ui.enter_template(g, k),
                                            None => {
                                                ui.leave_template();
                                                Ok(String::new())
                                            }
                                        };
                                        if let Err(e) = r {
                                            ui.message = Some((format!("{e:#}"), true));
                                        }
                                    }
                                    cx.notify();
                                },
                            )))
                        }),
                );
            }
        }
        if let Some((n_sel, node)) = ui
            .canvas
            .selected
            .and_then(|n| Some((n, ui.nodes.get(n)?)))
            .filter(|(_, n)| n.foreign)
        {
            let [x, y, w, h] = node.rect;
            // The template it is part of, by its name in the UI files, and
            // the container of this file that shows it.
            let template = node.owners.first().map(|&(g, _)| g);
            let template_name = template.map(|g| {
                let id = g.to_string();
                self.ui_find
                    .index
                    .as_ref()
                    .and_then(|i| i.entries.iter().find(|e| e.file == id))
                    .map_or_else(|| id[..8].to_owned(), |e| e.name.clone())
            });
            let owner = ui.movable(n_sel);
            body = body
                .child(section("Widget (from template)"))
                .child(info_row("Name", node.name.clone()))
                .child(info_row("Box", format!("{x:.0}, {y:.0} · {w:.0}×{h:.0}")))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(
                            "Part of a template. Double-click it (or Edit here) to edit the template in place; a drag here moves the container that shows it.",
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .when_some(owner, |d, o| {
                            d.child(
                                button(format!("Select {}", ui.nodes[o].name), false).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        if let Some(ui) = &mut this.ui {
                                            ui.select_widget(o);
                                        }
                                        cx.notify();
                                    }),
                                ),
                            )
                        })
                        .when_some(
                            node.owners.first().copied().zip(template_name),
                            |d, ((g, key), name)| {
                                let target = format!("{GENERATED}{g}");
                                d.child(
                                    button(format!("Edit {name} here"), false)
                                        .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                                            "Edit in place",
                                            "Double-click",
                                            "Edit the template with the screen around it",
                                        ))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(ui) = &mut this.ui {
                                                match ui.enter_template(g, key) {
                                                    Ok(m) => ui.message = Some((m, false)),
                                                    Err(e) => {
                                                        ui.message = Some((format!("{e:#}"), true))
                                                    }
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(button("Open file", false).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.follow(target.clone(), cx);
                                        cx.notify();
                                    },
                                )))
                            },
                        ),
                );
            // What feeds it, read only (see `data_section`).
            if let Some(d) = data.take() {
                body = body.child(d);
            }
            if let Some(block) = self.evaluated_block() {
                body = body.child(block);
            }
            body = body.children(screen_wide.iter_mut().filter_map(Option::take));
        }
        if let Some((n_sel, node)) = ui
            .canvas
            .selected
            .and_then(|n| Some((n, ui.nodes.get(n)?)))
            .filter(|(_, n)| !n.foreign)
        {
            let widgets = a.widgets();
            if let Some(widget) = widgets.get(node.widget) {
                let [x, y, w, h] = node.rect;
                body = body
                    .child(section("Widget"))
                    .child(info_row("Name", node.name.clone()))
                    .child(info_row("Class", ui.name(widget.class)))
                    .child(info_row("Box", format!("{x:.0}, {y:.0} · {w:.0}×{h:.0}")));
                // What feeds the widget, and binding more of it.
                if let Some(d) = data.take() {
                    body = body.child(d);
                }
                if let Some(block) = self.evaluated_block() {
                    body = body.child(block);
                }
                body = body.children(screen_wide.iter_mut().filter_map(Option::take));
                let layout = self.layout_block(&follows, cx);
                let has_layout = layout.is_some();
                if let Some(block) = layout {
                    body = body.child(block);
                }
                body = body.child(section("Properties"));
                // The Layout block edits these; listed twice they read as
                // two knobs for one thing.
                const IN_LAYOUT: [&str; 5] = [
                    "Position",
                    "PivotPoint",
                    "RotationAngle",
                    "Scale",
                    "AnchorPoint",
                ];
                let frame = ui.frames.get(n_sel).map(|f| f.bound);
                let mut raw = Vec::new();
                for prop in &widget.props {
                    match prop.name {
                        // Arrays and nested objects (Components, Effects,
                        // PropertyValues) have their own views.
                        None => raw.push(prop),
                        Some(_) if prop.value == Value::Other => raw.push(prop),
                        Some(name) if has_layout && IN_LAYOUT.contains(&name) => {}
                        Some(name) => {
                            body = body.child(self.prop_row(ui, prop, name.to_owned(), frame, cx));
                        }
                    }
                }
                // Fields the schema has no name for: kept for the format's
                // sake, rarely what a layout needs.
                if !raw.is_empty() {
                    let (heading, open) = gpui_editor_kit::widgets::collapsible(
                        "ui-raw-props",
                        format!("More fields · {}", raw.len()),
                        false,
                        window,
                        cx,
                    );
                    body = body.child(heading);
                    if open {
                        for prop in raw {
                            let label = prop
                                .name
                                .map_or_else(|| format!("{:08x}", prop.crc), str::to_owned);
                            body = body.child(self.prop_row(ui, prop, label, None, cx));
                        }
                    }
                }
            }
        }
        // The record a widget is stored as only matters in the records
        // view; next to the widget's own fields it was a hex dump of them.
        let widget_record = ui
            .canvas
            .selected
            .and_then(|n| ui.nodes.get(n))
            .filter(|n| !n.foreign)
            .and_then(|n| a.widgets().get(n.widget).map(|w| w.object));
        if let Some(span) = ui
            .selected
            .filter(|&s| ui.show_records || Some(s) != widget_record)
        {
            let s = &a.spans()[span];
            body = body
                .child(section("Record"))
                .child(info_row("Kind", ui.describe(span)))
                .child(info_row(
                    "Bytes",
                    format!(
                        "{:#x}..{:#x} ({} B)",
                        s.range.start,
                        s.range.end,
                        s.range.len()
                    ),
                ));
            if let Record::Color([r, g, b]) = s.record {
                body = body.child(
                    div()
                        .h(px(24.))
                        .w_full()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(theme::get().border)
                        .bg(rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))),
                );
            }
            if let Some(text) = ui.edit_text(span) {
                let editing = ui
                    .editing
                    .as_ref()
                    .filter(|e| e.field == span)
                    .map(|e| e.display());
                let filter = Filter::Text;
                body = body.child(
                    text_field(("ui-edit", span), &text, editing, "value").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.start_span_edit(span, Keys::Canvas, filter, window, cx)
                        },
                    )),
                );
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .child("Click to edit, Enter to apply, Escape to cancel. Ctrl S stages into the mod."),
                );
            }
            if let Record::Ref(r) = s.record {
                let target = format!("{GENERATED}{}", r.id);
                body = body.child(button("Open target", false).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.follow(target.clone(), cx);
                        cx.notify();
                    },
                )));
            }
            let bytes = &a.encode()[s.range.clone()];
            body = body.child(
                div()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child(hex(&bytes[..bytes.len().min(256)])),
            );
        }
        body = body.children(screen_wide.iter_mut().filter_map(Option::take));
        let props = a.properties();
        if !props.is_empty() {
            // What the template takes, apart from what its own bindings
            // compute (intermediate results, often dozens): each folds, so
            // a big template does not bury Game data under them.
            let table = a.bindings(&a.widgets());
            let computed: std::collections::HashSet<usize> = table
                .bindings
                .iter()
                .flat_map(|b| b.targets.iter())
                .filter_map(|&t| match table.slots.get(t) {
                    Some(anno_formats::phx::binding::Slot::Template(p, _)) => Some(*p),
                    _ => None,
                })
                .collect();
            let row = |prop: &anno_formats::phx::CustomProperty, cx: &mut Context<Self>| {
                let span = prop.default;
                // Name and value in words; the type only when it is known,
                // and an empty default as "not set".
                let value = property_default_text(a, prop);
                let value = if value == "none" {
                    "not set".to_owned()
                } else {
                    value
                };
                let label = match phx::property_type_name(prop.kind) {
                    Some(kind) => format!("{}: {value} ({kind})", a.property_name(prop)),
                    None => format!("{}: {value}", a.property_name(prop)),
                };
                button(label, ui.selected == Some(span))
                    .justify_start()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.selected = Some(span);
                        }
                        cx.notify();
                    }))
            };
            let (inputs, results): (Vec<(usize, &anno_formats::phx::CustomProperty)>, Vec<_>) =
                props
                    .iter()
                    .enumerate()
                    .partition(|(i, _)| !computed.contains(i));
            // The game data it is handed (an object input: `-Data`,
            // GameContext) apart from the values that set it up, as the
            // graph draws them (user, round 17).
            let (models, inputs): (Vec<_>, Vec<_>) = inputs
                .into_iter()
                .partition(|(_, p)| p.object_asset.is_some());
            for (id, title, list, open) in [
                ("ui-template-models", "View models", models, true),
                ("ui-template-inputs", "Template inputs", inputs, true),
                (
                    "ui-template-computed",
                    "Computed by its bindings",
                    results,
                    false,
                ),
            ] {
                if list.is_empty() {
                    continue;
                }
                let (heading, shown) = gpui_editor_kit::widgets::collapsible(
                    id,
                    format!("{title} · {}", list.len()),
                    open && list.len() <= 12,
                    window,
                    cx,
                );
                body = body.child(heading);
                if shown {
                    for (_, prop) in list {
                        body = body.child(row(prop, cx));
                    }
                }
            }
        }
        let refs = a.cross_refs();
        // File internals are for debugging the format, not for editing.
        let (heading, advanced) = gpui_editor_kit::widgets::collapsible(
            "ui-asset-advanced",
            "Advanced",
            false,
            window,
            cx,
        );
        body = body.child(heading);
        if advanced {
            body = body
                .child(info_row(
                    "Guid",
                    a.guid().map_or("-".into(), |g| g.to_string()),
                ))
                .child(info_row("Root class", ui.name(a.root_class())))
                .child(info_row("Classes", a.types().len().to_string()))
                .child(info_row(
                    "Root array size",
                    a.root_children().map_or("-".into(), |n| n.to_string()),
                ));
        }
        let mut refs_shown = false;
        if !refs.assets.is_empty() {
            let (heading, shown) = gpui_editor_kit::widgets::collapsible(
                "ui-references",
                format!("References · {}", refs.assets.len()),
                refs.assets.len() <= 6,
                window,
                cx,
            );
            body = body.child(heading);
            refs_shown = shown;
        }
        if refs_shown {
            // Component templates have no name of their own in the index:
            // what this file calls their instances ("UIScaling").
            let mut component_names: std::collections::HashMap<String, String> = Default::default();
            for w in a.widgets().iter() {
                for c in a.components(w).into_iter().chain(a.effects(w)) {
                    if let Some(g) = c.template {
                        component_names.entry(g.to_string()).or_insert(c.name);
                    }
                }
            }
            // The target's own name from the UI files index (its file name is
            // the GUID), with the class and short GUID muted after it: raw
            // "ColorPaletteAsset c63efd8b" read as debugger output. A linear
            // look-up is fine for the handful of references a screen has.
            let index = self.ui_find.index.clone();
            for (class, guid, count) in refs.assets.iter().take(64) {
                let target = format!("{GENERATED}{guid}");
                let id = guid.to_string();
                let name = index
                    .as_ref()
                    .and_then(|i| {
                        i.entries
                            .iter()
                            .find(|e| e.file.starts_with(id.as_str()))
                            .map(|e| e.name.clone())
                    })
                    // A file named by its GUID has the GUID for a name: then the
                    // class leads, and the GUID full width cut the class to
                    // "ColorP…".
                    .filter(|n| !n.to_ascii_lowercase().starts_with(&id[..8]))
                    .or_else(|| component_names.get(&id).filter(|n| !n.is_empty()).cloned());
                let short = &id[..8];
                let class_name = ui.name(*class).to_owned();
                let detail = match (&name, count) {
                    (Some(_), 1) => format!("{class_name} · {short}"),
                    (Some(_), n) => format!("{class_name} · {short} · ×{n}"),
                    (None, 1) => short.to_owned(),
                    (None, n) => format!("{short} · ×{n}"),
                };
                body = body.child(
                    div()
                        .id(SharedString::from(format!("ui-ref-{id}")))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(26.))
                        .rounded(px(3.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::get().hover))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(theme::get().text)
                                .child(name.unwrap_or(class_name)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(10.))
                                .text_color(theme::get().muted)
                                .child(detail),
                        )
                        .child(div().text_color(theme::get().accent).child("→"))
                        .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                            "Open",
                            "",
                            format!("{GENERATED}{id}"),
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.follow(target.clone(), cx);
                            cx.notify();
                        })),
                );
            }
        }
        if !refs.images.is_empty() {
            body = body.child(section(&format!("Images · {}", refs.images.len())));
            for image in refs.images.iter().take(64) {
                let target = image.clone();
                body = body.child(
                    button(image.rsplit('\\').next().unwrap_or(image).to_owned(), false)
                        .justify_start()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.follow(target.clone(), cx);
                            cx.notify();
                        })),
                );
            }
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(
                gpui_editor_kit::widgets::vscroll(
                    "ui-inspector-scroll",
                    &self.scrolls.get("ui-inspector"),
                    body,
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }
}

/// One bit per node, as a flat word array: where a `Vec<bool>` would be.
#[derive(Clone, Debug, Default)]
pub(super) struct Bits {
    words: Vec<u64>,
    len: usize,
}

impl Bits {
    pub(super) fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// False past the end.
    pub(super) fn get(&self, i: usize) -> bool {
        self.words
            .get(i / 64)
            .is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    pub(super) fn set(&mut self, i: usize, on: bool) {
        let bit = 1 << (i % 64);
        if on {
            self.words[i / 64] |= bit;
        } else {
            self.words[i / 64] &= !bit;
        }
    }
}

impl FromIterator<bool> for Bits {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        let mut bits = Bits::default();
        for (i, on) in iter.into_iter().enumerate() {
            if i % 64 == 0 {
                bits.words.push(0);
            }
            bits.words[i / 64] |= u64::from(on) << (i % 64);
            bits.len = i + 1;
        }
        bits
    }
}

#[cfg(test)]
mod tests {
    use super::{UiEditor, canvas, hex, parse_value};
    use crate::ui::preview::{FLAG_TESTS, game_flags, set_game_flag};
    use crate::ui::test_support::{TECH_TREE, install, key, open};
    use anno_formats::{
        phx::{Record, Value},
        vfs::Install,
    };
    use std::sync::Arc;

    // A burst of relayouts trims once, then not again for 2 s; a finished
    // probe trims at once.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn heap_release_is_throttled_but_a_probe_forces_it() {
        assert!(super::release_heap(true));
        assert!(!super::release_heap(false), "within 2 s");
        assert!(super::release_heap(true), "forced");
    }

    // While a field is typing, chords it ignores stop there; undo, redo,
    // save and the workspace keys still reach the editor; plain keys are
    // the field's to take or pass on.
    #[test]
    fn a_typing_field_keeps_every_chord_but_the_editors() {
        let keeps = |k| super::field_keeps(&key(k).keystroke);
        for k in [
            "ctrl-d",
            "ctrl-c",
            "ctrl-v",
            "ctrl-delete",
            "alt-up",
            "ctrl-g",
        ] {
            assert!(keeps(k), "{k}");
        }
        for k in ["ctrl-z", "ctrl-y", "ctrl-s", "ctrl-3", "f5", "delete", "a"] {
            assert!(!keeps(k), "{k}");
        }
    }

    #[test]
    fn installed_1800_ui_edits_save_into_a_separate_mod() {
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = Arc::new(Install::open(dir).unwrap());
        let game = anno_formats::game::Game::Anno1800;
        let path = "data/ui/studio/generated/006956b4-ff57-46d4-b278-0c4dbe18b722";
        let original = install.read(path).unwrap();
        let mut ui = UiEditor::load(install.clone(), path).unwrap();
        assert!(!ui.nodes.is_empty());
        assert!(!ui.texts.is_empty());
        let (span, value) = ui
            .asset
            .spans()
            .iter()
            .enumerate()
            .find_map(|(i, span)| match span.record {
                Record::Value(Value::Vec3(v)) => Some((i, v)),
                _ => None,
            })
            .unwrap();
        let text = format!("{}, {}, {}", value[0] + 10., value[1], value[2]);
        ui.commit(span, &text).unwrap();
        assert!(ui.dirty());
        let changed = ui.asset.bytes().to_vec();
        assert_ne!(original, changed);
        assert!(ui.undo(false));
        assert_eq!(ui.asset.bytes(), original);
        assert!(ui.undo(true));
        let config = tempfile::tempdir().unwrap();
        let mut project = crate::project::Project::load(config.path(), game);
        ui.stage(&mut project).unwrap();
        let folder = project.write().unwrap();
        assert_eq!(std::fs::read(folder.join(path)).unwrap(), changed);
        install.mount_mod(&folder).unwrap();
        let reopened = UiEditor::load(install.clone(), path).unwrap();
        assert_eq!(reopened.asset.bytes(), changed);
        // Mounting is only an editor overlay; shipped archive data stays intact.
        assert_eq!(
            install.read_file(install.file(path).unwrap()).unwrap(),
            original
        );
        let mut foreign =
            crate::project::Project::load(config.path(), anno_formats::game::Game::Anno117);
        assert!(ui.stage(&mut foreign).is_err());
    }

    #[test]
    fn installed_1800_unknown_schema_is_read_only() {
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = Arc::new(Install::open(dir).unwrap());
        let path = "data/ui/studio/generated/00dba1a6-05b4-464d-94f7-2f2bfeda2609";
        let mut ui = UiEditor::load(install, path).unwrap();
        assert!(!ui.editable());
        assert!((0..ui.asset.spans().len()).all(|i| ui.edit_text(i).is_none()));
        let err = ui.commit(0, "test").unwrap_err();
        assert!(err.to_string().contains("read-only"));
        let config = tempfile::tempdir().unwrap();
        let mut project =
            crate::project::Project::load(config.path(), anno_formats::game::Game::Anno1800);
        assert!(ui.stage(&mut project).is_err());
        assert!(project.files.is_empty());
    }

    // Needs the game: a string edit goes through history and undo gives the
    // file back; a canvas drag becomes a Position edit and the relayout
    // shows it moved; a colour takes only what reads as one.
    #[test]
    fn edits_go_through_history_and_validate_input() {
        let Some(mut ui) = open(TECH_TREE) else {
            return;
        };
        let original = ui.asset.encode();
        let text = ui
            .asset
            .strings()
            .find(|(_, s)| s.with_crc && s.text.len() > 3)
            .map(|(i, _)| i)
            .expect("an editable string");
        ui.commit(text, "RenamedByTest").unwrap();
        assert!(
            matches!(&ui.asset.spans()[text].record, Record::Str(s) if s.text == "RenamedByTest")
        );
        assert!(ui.dirty());
        assert!(ui.undo(false));
        assert_eq!(ui.asset.encode(), original);

        let (node, before) = ui
            .nodes
            .iter()
            .enumerate()
            .find(|(i, n)| n.image.is_some() && ui.frames[*i].position_span.is_some())
            .map(|(i, n)| (i, n.rect))
            .expect("a movable image widget");
        let widget = ui.nodes[node].widget;
        let mut after = before;
        after[0] += 25.;
        ui.canvas_change(canvas::Change {
            widget,
            before,
            after,
        });
        let rect = |ui: &UiEditor| ui.nodes.iter().find(|n| n.widget == widget).unwrap().rect;
        let moved = rect(&ui);
        assert!(
            (moved[0] - before[0] - 25.).abs() < 0.01,
            "{before:?} -> {moved:?}"
        );
        assert!(ui.undo(false));
        assert_eq!(rect(&ui), before);

        if let Some((color, _)) = ui
            .asset
            .spans()
            .iter()
            .enumerate()
            .find(|(_, s)| matches!(s.record, Record::Color(_)))
        {
            assert!(ui.commit(color, "zz").is_err());
            ui.commit(color, "#102030").unwrap();
            assert_eq!(
                ui.asset.spans()[color].record,
                Record::Color([0x10, 0x20, 0x30])
            );
        }
    }

    #[test]
    fn values_parse_to_their_own_kind() {
        assert_eq!(
            parse_value(&Value::Vec3([0.; 3]), "1, 2,3").unwrap(),
            Value::Vec3([1., 2., 3.])
        );
        assert!(parse_value(&Value::Vec2([0.; 2]), "1").is_err());
        assert_eq!(
            parse_value(&Value::Bool(false), "yes").unwrap(),
            Value::Bool(true)
        );
        assert!(parse_value(&Value::Str("a".into()), "b").is_err());
        assert_eq!(hex(&[1, 2]), "01 02");
    }

    // Needs the game: a Game data flag that shows more widgets renumbers
    // the canvas nodes; the selection stays on the widget it was on
    // (round 15: the inspector jumped to an InfoTip that came into view
    // before the selected button). A list's sample items repeat one
    // prototype widget: the second or third objective's Icon stays
    // selected, the same item, when flags show widgets before it (round
    // 16: QuestChoiceData moved the selection to the first objective's
    // Icon). Who it is comes from the layout (the instances it sits in)
    // and where it is drawn, not from the key the editor finds it by.
    #[test]
    fn the_selection_stays_on_its_widget_when_a_flag_shows_more() {
        let _flags = FLAG_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut ui) = open("questtrackerscene") else {
            return;
        };
        let mut moved = false;
        for (path, on, _) in game_flags() {
            // The last widget drawn: anything a flag shows comes before it.
            let pick = ui.nodes.len() - 1;
            ui.select_widget(pick);
            // Who it is by the layout: its widget in the instances it sits in.
            let who = |n: &canvas::Node| (n.widget, n.foreign, n.owners.clone());
            let was = who(&ui.nodes[pick]);
            set_game_flag(&path, Some(!on));
            ui.relayout();
            let now = (0..ui.nodes.len()).find(|&n| who(&ui.nodes[n]) == was);
            if let Some(now) = now.filter(|&n| n != pick) {
                moved = true;
                assert_eq!(ui.canvas.selected, Some(now), "{path}: the same widget");
            }
            set_game_flag(&path, None);
            ui.relayout();
            if moved {
                break;
            }
        }
        assert!(moved, "a flag that shows widgets before the selection");

        // Every path of that name: the flags listed are every screen's the
        // tests have laid out so far, not this one's alone.
        let flag = |end: &str| -> Vec<String> {
            let paths: Vec<String> = game_flags()
                .into_iter()
                .map(|f| f.0)
                .filter(|p| p.ends_with(end))
                .collect();
            assert!(!paths.is_empty(), "no flag {end}");
            paths
        };
        let set = |paths: &[String], on: Option<bool>| {
            for p in paths {
                set_game_flag(p, on);
            }
        };
        let (timer, choice) = (
            flag("TimerData.IsVisible"),
            flag("QuestChoiceData.IsVisible"),
        );
        set(&timer, Some(true));
        ui.relayout();
        // The objectives' Icons: one column of equal boxes, top to bottom.
        let column = |ui: &UiEditor, n: usize| -> Vec<usize> {
            let r = ui.nodes[n].rect;
            let mut c: Vec<usize> = (0..ui.nodes.len())
                .filter(|&m| {
                    let q = ui.nodes[m].rect;
                    ui.nodes[m].name == ui.nodes[n].name
                        && ui.nodes[m].visible
                        && (q[0] - r[0]).abs() < 0.5
                        && (q[2] - r[2]).abs() < 0.5
                        && (q[3] - r[3]).abs() < 0.5
                })
                .collect();
            c.sort_by(|&a, &b| ui.nodes[a].rect[1].total_cmp(&ui.nodes[b].rect[1]));
            c
        };
        let first = (0..ui.nodes.len())
            .find(|&n| ui.nodes[n].name.as_ref() == "Icon" && column(&ui, n).len() >= 3)
            .expect("a column of three Icons");
        for rank in [1, 2] {
            let pick = column(&ui, first)[rank];
            ui.select_widget(pick);
            let owners = ui.nodes[pick].owners.clone();
            set(&choice, Some(true));
            ui.relayout();
            let now = ui.canvas.selected.expect("still selected");
            assert_ne!(now, pick, "the flag shows widgets before it");
            assert_eq!(
                ui.nodes[now].owners, owners,
                "item {rank}: the same instances"
            );
            assert_eq!(column(&ui, now).iter().position(|&n| n == now), Some(rank));
            set(&choice, None);
            ui.relayout();
            let back = ui.canvas.selected.expect("still selected");
            assert_eq!(
                column(&ui, back).iter().position(|&n| n == back),
                Some(rank)
            );
        }
        set(&timer, None);
    }

    // Needs the game: a native enum value (a label's HorizontalOverflow)
    // shows its entry's name and takes it typed, any case; a name it does
    // not have is refused with the ones it has.
    #[test]
    fn native_enum_values_show_and_take_names() {
        let Some(install) = install() else {
            return;
        };
        anno_formats::phx::register_sources(&install);
        let ty = anno_formats::phx::name_crc(b"HorizontalOverflow");
        let v = |value| Value::Enum { ty, value };
        assert_eq!(super::value_text(&v(4)).as_deref(), Some("Ellipsis"));
        assert_eq!(parse_value(&v(0), "wrap").unwrap(), v(1));
        assert_eq!(parse_value(&v(0), "3").unwrap(), v(3));
        let said = parse_value(&v(0), "sideways").unwrap_err().to_string();
        assert!(
            said.starts_with("HorizontalOverflow takes DeprecatedAutosize, Wrap"),
            "{said}"
        );
    }
}
