// Owns the cfg open in the Model workspace: its document and undo ring, the
// model and GPU pieces built from it, the selection, and staging it into
// the project. The viewport is in `view`; the hierarchy, inspector and
// timeline panes in `hierarchy`, `inspector` and `timeline`.

pub mod effect;
pub mod fields;
pub mod hierarchy;
pub mod inspector;
pub mod keys;
pub mod lights;
pub mod list;
pub mod picker;
pub mod playback;
pub mod sky;
pub mod sound;
pub mod templates;
pub mod timeline;
pub mod view;

#[cfg(test)]
mod tests;
// Each pane's window tests, beside the shared ones in `tests`.
#[cfg(test)]
mod hierarchy_tests;
#[cfg(test)]
mod templates_tests;
#[cfg(test)]
mod timeline_tests;
#[cfg(test)]
mod view_tests;

use crate::editor::{Editor, Workspace};
use crate::island::models::{self, Cache, Piece};
use anno_core::files::ModelLook;
use anno_core::game::Game;
use anno_formats::efc::Efc;
use anno_formats::model::{self as cfg, BoneLink, Model, Rig, Transform};
use anno_formats::rdm::Animation;
use anno_formats::rdp::Rdp;
use anno_formats::vfs::Install;
use anno_formats::xml::{Doc, Id};
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    field::TextEdit,
    gizmo,
    gpui::prelude::*,
    history::History,
    math::{self, Mat4},
    tree::Tree,
};
use std::sync::Arc;

/// Models Open model lists as recent, and the File menu up to its own cap.
const RECENT_MODELS: usize = 12;

/// The lists of a cfg's MAIN config the hierarchy shows, in its order,
/// with what it calls them.
pub const LISTS: [(&str, &str); 11] = [
    ("Models", "Meshes"),
    // 2070's trees: stems as instanced models, leaves as clusters.
    ("InstancingModels", "Stems"),
    ("InstancingBBClusters", "Leaf clusters"),
    ("Files", "Props"),
    ("Decals", "Decals"),
    ("TerrainDecals", "Decals"),
    ("Particles", "Particles"),
    // 2070's effects (`.efc` emitters, not baked `.rdp`).
    ("Effects", "Effects"),
    ("Lights", "Lights"),
    ("Sequences", "Sequences"),
    ("Clothes", "Cloth"),
];

/// What the inspector is typing into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// A leaf's text.
    Value(Id),
    /// The selected entry's placement, by axis.
    Position(usize),
    /// Degrees about X, Y, Z (applied Y, then X, then Z).
    Rotation(usize),
    Scale(usize),
    /// The selected particle entry's effect: a number by [`inspector`]'s list
    /// of them (frame time, boosts, the emitter's values).
    Effect(u8),
    /// Material `k`'s texture path of the selected effect.
    EffectTexture(usize),
    /// The selected entry's emitter's texture: an effect made in the
    /// editor, or a 2070 `.efc` emitter.
    EmitterTexture,
    /// The picked joint's key at the playhead: degrees about X, Y, Z, and
    /// its offset from its parent.
    JointTurn(usize),
    JointMove(usize),
    /// The inspector's field filter.
    InspectorFilter,
    /// Seconds a new animation lasts.
    NewAnimationLength,
    /// The hierarchy's search.
    HierarchyFilter,
}

/// The names a packed `SequenceID` (`action << 24 | (info + 52) << 8 |
/// variant`) stands for, from the game's own Lua enums
/// (`datasets.SequenceAction`, `datasets.SequenceInfo`). Empty where the
/// game ships none (1800), and ids then read as numbers.
#[derive(Clone, Debug, Default)]
pub struct SequenceNames {
    actions: Vec<(i64, String)>,
    infos: Vec<(i64, String)>,
}

impl SequenceNames {
    const ACTIONS: &str = "data/script/types/generated/datasets/sequence_action.lua";
    const INFOS: &str = "data/script/types/generated/datasets/sequence_info.lua";

    pub fn read(install: &Install) -> SequenceNames {
        // `Name = 12,` lines of a generated Lua table.
        let table = |path: &str| -> Vec<(i64, String)> {
            let Ok(bytes) = install.read(path) else {
                return Vec::new();
            };
            (String::from_utf8_lossy(&bytes).lines())
                .filter_map(|l| {
                    let (name, value) = l.trim().trim_end_matches(',').split_once('=')?;
                    let name = name.trim();
                    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                        return None;
                    }
                    Some((value.trim().parse().ok()?, name.to_owned()))
                })
                .collect()
        };
        SequenceNames {
            actions: table(Self::ACTIONS),
            infos: table(Self::INFOS),
        }
    }

    /// "Neutral Idle 1" for 100691457; the number where a part has no name.
    pub fn name(&self, id: u32) -> String {
        let (action, info, variant) = (
            i64::from(id >> 24),
            i64::from((id >> 8) & 0xffff) - 52,
            id & 0xff,
        );
        let find =
            |t: &[(i64, String)], v: i64| t.iter().find(|(k, _)| *k == v).map(|(_, n)| n.clone());
        match (find(&self.actions, action), find(&self.infos, info)) {
            (Some(a), Some(i)) => format!("{a} {i} {variant}"),
            _ => format!("Sequence {id}"),
        }
    }
}

/// What a model load hands the editor, built off the UI thread.
pub struct Loaded {
    pub path: String,
    pub doc: Doc,
    /// Read from the project, not the game.
    pub from_project: bool,
    /// Whose model: its lights light by that game's rules.
    game: Game,
    model: Model,
    pieces: Vec<Piece>,
    cache: Arc<Cache>,
    names: SequenceNames,
}

impl Loaded {
    /// Reads `path` from the project when it holds its own copy, else from
    /// the game, and builds its pieces.
    pub fn read(
        install: &Install,
        look: &ModelLook,
        game: Game,
        path: &str,
        project_copy: Option<Vec<u8>>,
    ) -> Result<Loaded> {
        let from_project = project_copy.is_some();
        let bytes = match project_copy {
            Some(b) => b,
            None => install
                .read(path)
                .with_context(|| format!("reading {path}"))?,
        };
        let doc = Doc::parse(&bytes);
        let cache = Arc::new(Cache::new(look));
        let mut model = Model::load_doc(install, path, Some(&doc))?;
        let pieces = own_pieces(install, &cache, &mut model);
        Ok(Loaded {
            names: SequenceNames::read(install),
            path: path.to_owned(),
            doc,
            from_project,
            game,
            model,
            pieces,
            cache,
        })
    }
}

/// The pieces of `model`'s own parts and decals; its props load apart
/// ([`Nested`]).
fn own_pieces(install: &Install, cache: &Cache, model: &mut Model) -> Vec<Piece> {
    let props = std::mem::take(&mut model.props);
    let pieces = models::build(install, cache, model, 1., true);
    model.props = props;
    pieces
}

/// Prop cfgs loading at once: each read takes a core while the UI keeps
/// its own, and a building nests up to a few dozen.
const NESTED_LOADS: usize = 4;

/// A prop cfg the model places, built once at its own origin off the UI
/// thread and drawn wherever an entry places it. Its entries' terrain
/// switches change the meshes built, so they are part of what it is.
struct Nested {
    cfg: String,
    deform: bool,
    follow: bool,
    load: Load,
}

enum Load {
    Queued,
    Running,
    /// Empty when the cfg did not read (shipped cfgs name ~130 missing
    /// prop cfgs; `Model::problems` lists those).
    Ready(Arc<Vec<Piece>>),
}

/// What undo keeps: the cfg and the effects, emitters and animations
/// edited with it.
#[derive(Clone, Default)]
struct Snapshot {
    doc: Doc,
    effects: Vec<(String, Arc<Rdp>)>,
    efcs: Vec<(String, Arc<Efc>)>,
    emitters: Vec<effect::Authored>,
    animations: Vec<(String, Arc<Animation>)>,
}

pub struct ModelEditor {
    /// The cfg's virtual path, where the project saves it.
    pub path: String,
    pub doc: Doc,
    /// Effects edited or made this session, by path: saved beside the cfg.
    pub effects: Vec<(String, Arc<Rdp>)>,
    /// 2070 `.efc` effects edited this session, by path: saved beside the
    /// cfg.
    pub efcs: Vec<(String, Arc<Efc>)>,
    /// Which emitter of the selected entry's `.efc` the inspector edits.
    pub emitter_slot: usize,
    /// The emitters of effects made in the editor, this session's and the
    /// project's.
    pub(crate) emitters: Vec<effect::Authored>,
    /// What the project held at each effect path before this session first
    /// saved an edit there, so a save after undoing the edit puts it back.
    files_before: Vec<(String, Option<Vec<u8>>)>,
    /// Animations edited this session, by path: saved beside the cfg.
    pub animations: Vec<(String, Arc<Animation>)>,
    /// Opens saved: a game original only becomes unsaved with an edit, so
    /// opening another model or leaving does not ask about a file nobody
    /// changed.
    history: History<Snapshot>,
    /// The project holds this cfg: it was read from there or saved there.
    pub in_project: bool,
    /// The doc as the game reads it, meshes loaded; rebuilt after edits.
    pub model: Model,
    /// The model's own pieces first, then its props' as far as loaded.
    pub pieces: Vec<Piece>,
    /// How many of `pieces` are the model's own.
    own: usize,
    nested: Vec<Nested>,
    /// Props of the opening model are still loading: they frame the view
    /// as they land, until someone turns it.
    framing: bool,
    pub selected: Option<Id>,
    /// Which lists and entries are open in the hierarchy.
    pub tree: Tree<hierarchy::Row>,
    /// The joint picked in the hierarchy, by its part's entry: what Attach
    /// hangs the selected entry on.
    pub joint: Option<(Id, u16)>,
    pub typing: Option<TextEdit<Field>>,
    pub gizmo: gizmo::Mode,
    pub view: view::View,
    pub playback: playback::Playback,
    player: playback::Player,
    /// The cfg's lights as they light the view, and their clock.
    pub lights: lights::Lights,
    install: Arc<Install>,
    cache: Arc<Cache>,
    pub message: Option<(String, bool)>,
    /// Narrows the inspector's fields to names holding it.
    pub field_filter: String,
    /// Seconds the inspector's New animation makes.
    pub new_animation_length: f32,
    /// Sound clips the playhead crossed since the editor last played them.
    pub sounds_due: Vec<(Id, anno_formats::model::Sound)>,
    /// Where the playhead stood after the last frame that played sound;
    /// `None` while no clip's sound runs. A playhead found elsewhere moved
    /// other than by playing, which stops them.
    pub sound_clock: Option<(playback::Source, f32)>,
    /// The game's sounds once loaded (the editor's, shared): names sound
    /// clips and lists what one can play. `None` while they load, and for
    /// 2070.
    pub sounds: Option<Arc<anno_formats::audio::Library>>,
    /// The hierarchy's search and open row menu; the timeline's zoom,
    /// scroll and Add menu.
    pub hierarchy: hierarchy::Pane,
    pub timeline: timeline::Pane,
    pub names: SequenceNames,
}

impl ModelEditor {
    pub fn new(install: Arc<Install>, look: &ModelLook, loaded: Loaded) -> ModelEditor {
        let mut editor = ModelEditor {
            path: loaded.path,
            history: History::new(true),
            in_project: loaded.from_project,
            doc: loaded.doc,
            effects: Vec::new(),
            efcs: Vec::new(),
            emitter_slot: 0,
            emitters: Vec::new(),
            files_before: Vec::new(),
            animations: Vec::new(),
            model: loaded.model,
            own: loaded.pieces.len(),
            pieces: loaded.pieces,
            nested: Vec::new(),
            framing: true,
            selected: None,
            tree: Tree::default(),
            joint: None,
            typing: None,
            gizmo: gizmo::Mode::Move,
            view: view::View::new(look),
            playback: playback::Playback::default(),
            player: playback::Player::default(),
            lights: lights::Lights::new(loaded.game),
            install,
            cache: loaded.cache,
            message: None,
            field_filter: String::new(),
            new_animation_length: 2.,
            sounds_due: Vec::new(),
            sound_clock: None,
            sounds: None,
            hierarchy: Default::default(),
            timeline: Default::default(),
            names: loaded.names,
        };
        if let Some(main) = editor.main() {
            for (list, _) in LISTS {
                if let Some(l) = editor.doc.child(main, list) {
                    editor.tree.expand(hierarchy::Row::List(l));
                }
            }
        }
        editor.place_props();
        editor.framing = !editor.nested.is_empty();
        editor.lights.read(&editor.doc, &editor.model);
        editor.view.show(&editor.pieces, &editor.model, None, true);
        editor.player.bind(&editor.pieces, &editor.model);
        (editor.player).bind_cloth(&editor.doc, &editor.install, &editor.cache);
        // An animated model opens on its first sequence, paused; a still
        // one is posed once, which hands the view its lights.
        match editor.model.sequences.is_empty() {
            false => editor.play(playback::Source::Sequence(0)),
            true => editor.pose(),
        }
        editor
    }

    pub fn dirty(&self) -> bool {
        self.history.dirty()
    }

    /// The MAIN config of a cfg, or a prp's `Prop`.
    pub fn main(&self) -> Option<Id> {
        let doc = &self.doc;
        let configs = || doc.children(Id::ROOT).filter(|&c| doc.name(c) == "Config");
        configs()
            .find(|&c| (doc.child(c, "ConfigType")).is_some_and(|t| doc.text(t) == "MAIN"))
            .or_else(|| configs().next())
            .or_else(|| doc.child(Id::ROOT, "Prop"))
    }

    /// The entries of `list` under the MAIN config, in file order.
    pub fn entries(&self, list: &str) -> Vec<Id> {
        let doc = &self.doc;
        (self.main().and_then(|m| doc.child(m, list)))
            .map(|l| {
                doc.children(l)
                    .filter(|&c| doc.name(c) == "Config")
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The list an entry belongs to, when it is one of a MAIN list's.
    pub fn list_of(&self, entry: Id) -> Option<&'static str> {
        let list = self.doc.parent(entry)?;
        (self.doc.parent(list)? == self.main()?).then_some(())?;
        let name = self.doc.name(list);
        LISTS.iter().find(|(l, _)| *l == name).map(|(l, _)| *l)
    }

    /// What the hierarchy calls an entry: its name, else its file's, else
    /// its kind.
    pub fn label(&self, entry: Id) -> String {
        let doc = &self.doc;
        if self.list_of(entry) == Some("Sequences") {
            let id = doc
                .child(entry, "SequenceID")
                .and_then(|c| doc.text(c).parse().ok());
            return id.map_or_else(|| "sequence".to_owned(), |id| self.names.name(id));
        }
        let text = |name| {
            doc.child(entry, name)
                .map(|c| doc.text(c))
                .filter(|t| !t.is_empty())
        };
        if let Some(name) = text("Name") {
            return name.to_owned();
        }
        if let Some(file) = text("FileName") {
            let file = file.rsplit(['/', '\\']).next().unwrap_or(file);
            return file.to_owned();
        }
        text("ConfigType").unwrap_or("entry").to_lowercase()
    }

    /// Records the doc before an edit. Drags record once, at their start.
    fn checkpoint(&mut self) {
        self.history.checkpoint_owned(Snapshot {
            doc: self.doc.clone(),
            effects: self.effects.clone(),
            efcs: self.efcs.clone(),
            emitters: self.emitters.clone(),
            animations: self.animations.clone(),
        });
    }

    pub fn undo(&mut self, redo: bool) -> bool {
        let mut now = Snapshot {
            doc: std::mem::take(&mut self.doc),
            effects: std::mem::take(&mut self.effects),
            efcs: std::mem::take(&mut self.efcs),
            emitters: std::mem::take(&mut self.emitters),
            animations: std::mem::take(&mut self.animations),
        };
        let done = match redo {
            true => self.history.redo(&mut now),
            false => self.history.undo(&mut now),
        };
        self.doc = now.doc;
        (self.effects, self.efcs) = (now.effects, now.efcs);
        (self.emitters, self.animations) = (now.emitters, now.animations);
        if done {
            (self.player).reset_edits(&self.effects, &self.efcs, &self.animations);
            self.typing = None;
            if self.selected.is_some_and(|s| !self.doc.is_attached(s)) {
                self.selected = None;
            }
            self.rebuild();
        }
        done
    }

    /// Reads the model from the doc again and rebuilds its own pieces;
    /// meshes and textures come from the cache when unchanged. Props draw
    /// from their loads, and a prop cfg new to the model queues one.
    pub fn rebuild(&mut self) {
        match Model::load_doc(&self.install, &self.path, Some(&self.doc)) {
            Ok(mut model) => {
                self.pieces = own_pieces(&self.install, &self.cache, &mut model);
                self.own = self.pieces.len();
                self.model = model;
                self.place_props();
            }
            Err(e) => self.message = Some((format!("Model not read: {e:#}"), true)),
        }
        self.lights.read(&self.doc, &self.model);
        self.view
            .show(&self.pieces, &self.model, self.selected, false);
        self.player.bind(&self.pieces, &self.model);
        (self.player).bind_cloth(&self.doc, &self.install, &self.cache);
        // A sequence edited or removed: its clips may have moved.
        let source = match self.playback.source {
            playback::Source::Sequence(k) if k >= self.model.sequences.len() => {
                playback::Source::Still
            }
            s => s,
        };
        let time = self.playback.time;
        self.play(source);
        self.playback.time = time;
        self.pose();
    }

    /// The props' pieces after the model's own, each loaded prop cfg
    /// placed by every entry naming it; a cfg not yet loading queues.
    fn place_props(&mut self) {
        self.pieces.truncate(self.own);
        for prop in &self.model.props {
            let (deform, follow) = (prop.deform_to_terrain, prop.adapt_terrain_height);
            let found = (self.nested.iter())
                .find(|n| n.cfg == prop.cfg && n.deform == deform && n.follow == follow);
            let Some(n) = found else {
                self.nested.push(Nested {
                    cfg: prop.cfg.clone(),
                    deform,
                    follow,
                    load: Load::Queued,
                });
                continue;
            };
            let Load::Ready(pieces) = &n.load else {
                continue;
            };
            // On a joint the placement is in the joint's space: playback
            // poses the pieces by their entry, as it does a part's.
            let at = prop.transform.matrix();
            self.pieces.extend(pieces.iter().map(|p| Piece {
                local: math::multiply(&at, &p.local),
                element: Some(prop.element),
                ..p.clone()
            }));
        }
    }

    /// The props still to load, marked loading: as many as may start with
    /// those running.
    fn start_nested(&mut self) -> Vec<(String, bool, bool)> {
        let running = (self.nested.iter())
            .filter(|n| matches!(n.load, Load::Running))
            .count();
        (self.nested.iter_mut())
            .filter(|n| matches!(n.load, Load::Queued))
            .take(NESTED_LOADS.saturating_sub(running))
            .map(|n| {
                n.load = Load::Running;
                (n.cfg.clone(), n.deform, n.follow)
            })
            .collect()
    }

    /// A prop cfg's pieces, loaded: drawn where its entries place it. While
    /// the opening model's props land, the view fits them as long as the
    /// camera stands where the model opened.
    fn nested_loaded(&mut self, key: (String, bool, bool), pieces: Result<Vec<Piece>>) {
        let found = (self.nested.iter_mut())
            .find(|n| (&n.cfg, n.deform, n.follow) == (&key.0, key.1, key.2));
        let Some(n) = found else { return };
        let pieces = pieces.unwrap_or_else(|e| {
            eprintln!("{}: {e:#}", key.0);
            Vec::new()
        });
        n.load = Load::Ready(Arc::new(pieces));
        self.place_props();
        let settled = !(self.nested.iter()).any(|n| matches!(n.load, Load::Queued | Load::Running));
        let frame = self.framing && self.view.at_home();
        self.framing &= frame && !settled;
        (self.view).show(&self.pieces, &self.model, self.selected, frame);
        self.player.bind(&self.pieces, &self.model);
        self.pose();
    }

    pub fn select(&mut self, entry: Option<Id>) {
        if self.selected != entry {
            self.emitter_slot = 0;
        }
        self.selected = entry;
        self.typing = None;
        if let Some(e) = entry
            && let Some(list) = self.doc.parent(e)
        {
            self.tree.expand(hierarchy::Row::List(list));
        }
        self.view.outline(entry);
        // The skeleton overlay follows the selected part.
        self.pose();
    }

    /// The selected entry's placement, when it has one to edit.
    pub fn placement(&self, entry: Id) -> Option<Transform> {
        self.list_of(entry)
            .filter(|l| *l != "Sequences")
            .map(|_| cfg::placement(&self.doc, entry))
    }

    /// Moves `entry` to `t`. `record` is false while a drag repeats it:
    /// the drag recorded the doc when it began.
    pub fn set_placement(&mut self, entry: Id, t: Transform, record: bool) {
        let Some(old) = self.placement(entry) else {
            return;
        };
        if record {
            self.checkpoint();
        }
        cfg::set_placement(&mut self.doc, entry, &t);
        // A cloth starts again where it now hangs.
        if self.list_of(entry) == Some("Clothes") {
            (self.player).bind_cloth(&self.doc, &self.install, &self.cache);
        }
        self.moved(entry, &old, &t);
    }

    /// Carries a placement change into the pieces and model without a
    /// rebuild: every piece under the entry turns by the same matrix.
    fn moved(&mut self, entry: Id, old: &Transform, new: &Transform) {
        let Some(inverse) = math::inverse(&old.matrix()) else {
            return self.rebuild();
        };
        let delta = math::multiply(&new.matrix(), &inverse);
        for p in self.pieces.iter_mut().filter(|p| p.element == Some(entry)) {
            p.local = math::multiply(&delta, &p.local);
        }
        let m = &mut self.model;
        let transforms = (m.parts.iter_mut().map(|p| (p.element, &mut p.transform)))
            .chain(m.props.iter_mut().map(|p| (p.element, &mut p.transform)))
            .chain(m.decals.iter_mut().map(|p| (p.element, &mut p.transform)))
            .chain(
                m.particles
                    .iter_mut()
                    .map(|p| (p.element, &mut p.transform)),
            )
            .chain(m.lights.iter_mut().map(|p| (p.element, &mut p.transform)));
        for (_, t) in transforms.filter(|(e, _)| *e == entry) {
            *t = *new;
        }
        self.view
            .show(&self.pieces, &self.model, self.selected, false);
        self.pose();
    }

    /// Sets a leaf's text and rebuilds what it changes.
    pub fn set_value(&mut self, leaf: Id, text: &str) {
        if self.doc.text(leaf) == text.trim() {
            return;
        }
        self.checkpoint();
        self.doc.set_text(leaf, text);
        self.rebuild();
    }

    /// Sets a clip's start and end (ms as text) and replays it. A drag
    /// records once, at its first move.
    pub fn retime(&mut self, start: Id, end: Id, from: &str, to: &str, record: bool) {
        if record {
            self.checkpoint();
        }
        self.doc.set_text(start, from);
        self.doc.set_text(end, to);
        match Model::load_doc(&self.install, &self.path, Some(&self.doc)) {
            // The meshes stay: a retime changes only the sequences.
            Ok(model) => self.model.sequences = model.sequences,
            Err(e) => self.message = Some((format!("Model not read: {e:#}"), true)),
        }
        let time = self.playback.time;
        self.play(self.playback.source);
        self.playback.time = time;
        self.pose();
    }

    /// What an entry's transformer rigs it with.
    pub fn rig(&self, entry: Id) -> Option<Rig> {
        let m = &self.model;
        (m.parts.iter().map(|p| (p.element, p.rig)))
            .chain(m.props.iter().map(|p| (p.element, p.rig)))
            .chain(m.lights.iter().map(|p| (p.element, p.rig)))
            .chain(m.particles.iter().map(|p| (p.element, p.rig)))
            .find(|(e, _)| *e == entry)
            .map(|(_, r)| r)
    }

    /// The link to joint `j` of part entry `part`.
    pub fn link_to(&self, part: Id, j: u16) -> Option<BoneLink> {
        let k = self.model.parts.iter().position(|p| p.element == part)?;
        let skeleton = self.model.parts[k].mesh.as_ref()?.skeleton.as_ref()?;
        Some(BoneLink {
            model: k as u32,
            bone: *skeleton.hashes.get(j as usize)?,
        })
    }

    /// Hangs `entry` on a joint, or takes it off with `None`, keeping it
    /// where it shows in the bind pose: its placement turns into the new
    /// joint's space (or the model's).
    pub fn attach(&mut self, entry: Id, link: Option<BoneLink>) {
        let Some(at) = self.placement(entry) else {
            return;
        };
        let bind = |l: BoneLink| playback::joint(&self.model, &[], l);
        let old = self.rig(entry).and_then(|r| r.link).and_then(bind);
        let new = link.and_then(bind);
        if link.is_some() && new.is_none() {
            self.message = Some(("That joint is not in a loaded skeleton".into(), true));
            return;
        }
        let shown = match old {
            Some(j) => math::multiply(&j, &at.matrix()),
            None => at.matrix(),
        };
        let local = match new.and_then(|j| math::inverse(&j)) {
            Some(inverse) => math::multiply(&inverse, &shown),
            None => shown,
        };
        self.checkpoint();
        cfg::set_bone_link(&mut self.doc, entry, link);
        cfg::set_placement(&mut self.doc, entry, &Transform::from_matrix(&local));
        self.rebuild();
    }

    /// Adds an animation rdm to part entry `part`'s `<Animations>`, last:
    /// its `AnimationID` is the list's old length.
    pub fn add_animation(&mut self, part: Id, file: &str) -> Result<()> {
        let file = file.trim();
        anyhow::ensure!(file.ends_with(".rdm"), "an animation is an .rdm");
        anyhow::ensure!(
            self.install.exists(file),
            "{file} is not in the game or the mod"
        );
        self.checkpoint();
        self.push_animation(part, file);
        self.rebuild();
        Ok(())
    }

    /// Appends an `ANIMATION` entry playing `file` once at its own pace to
    /// part entry `part`'s `<Animations>`, made when it has none.
    fn push_animation(&mut self, part: Id, file: &str) {
        let list = match self.doc.child(part, "Animations") {
            Some(l) => l,
            None => self.doc.insert(part, None, "<Animations></Animations>"),
        };
        let xml = format!(
            "<Config><ConfigType>ANIMATION</ConfigType><FileName>{file}</FileName><PresetName></PresetName><LoopCount>0</LoopCount><Scale>1</Scale></Config>"
        );
        self.doc.insert(list, None, &xml);
    }

    /// A new sequence that plays part `part`'s animation `animation` once
    /// through, looped, as the game's idle: its id is Neutral Idle 1 until
    /// the inspector changes it. The clip spans what the game fits it to,
    /// `duration × Scale × max(LoopCount, 1)`.
    pub fn new_sequence(&mut self, part: usize, animation: usize) -> Result<()> {
        let main = self.main().context("the cfg has no MAIN config")?;
        let p = self.model.parts.get(part).context("no such part")?;
        let path = p
            .animations
            .get(animation)
            .cloned()
            .context("no such animation")?;
        let scale = p.animation_scales.get(animation).copied().unwrap_or(1.);
        let loops = p
            .animation_loops
            .get(animation)
            .copied()
            .unwrap_or(0)
            .max(1);
        let anim = self
            .player
            .animation(&self.install, &path)
            .context("the animation does not read")?;
        let end = (anim.duration * scale * loops as f32 * 1000.).round() as u32;
        self.checkpoint();
        let list = match self.doc.child(main, "Sequences") {
            Some(l) => l,
            None => self.doc.insert(main, None, "<Sequences></Sequences>"),
        };
        let xml = format!(
            "<Config><ConfigType>SEQUENCE</ConfigType>\n<SequenceID>100691457</SequenceID>\n<Speed>1</Speed>\n<EffectRestartOffset>0</EffectRestartOffset>\n<Looped>1</Looped>\n<DisableEffects>0</DisableEffects>\n<RestartEffects>0</RestartEffects>\n<Track><TrackID>1</TrackID>\n<TrackElement><Type>0</Type>\n<StartTime>0</StartTime>\n<EndTime>{end}</EndTime>\n<ModelID>{part}</ModelID>\n<AnimationID>{animation}</AnimationID>\n</TrackElement>\n</Track>\n</Config>"
        );
        let entry = self.doc.insert(list, None, &xml);
        self.tree.expand(hierarchy::Row::List(list));
        self.rebuild();
        let k = self.model.sequences.iter().position(|s| s.element == entry);
        self.play(k.map_or(playback::Source::Still, playback::Source::Sequence));
        self.select(Some(entry));
        Ok(())
    }

    /// A copy of `entry` right after it, selected.
    pub fn duplicate(&mut self, entry: Id) {
        self.checkpoint();
        // Sequences and bone links name meshes and particles by place.
        let old = self.list_of(entry).map(|l| (l, self.entries(l)));
        let copy = self.doc.duplicate(entry);
        if let Some((list, old)) = old {
            self.follow_places(list, &old);
        }
        self.rebuild();
        self.select(Some(copy));
    }

    pub fn remove(&mut self, entry: Id) {
        self.checkpoint();
        let old = self.list_of(entry).map(|l| (l, self.entries(l)));
        self.doc.remove(entry);
        if let Some((list, old)) = old {
            self.follow_places(list, &old);
        }
        if self.selected == Some(entry) {
            self.selected = None;
        }
        self.rebuild();
    }

    /// Moves `entry` before `before` in its list, or last, as one edit.
    /// Tracks and bone links name meshes and particles by their place in
    /// their list (`ModelID`, `ParticleID`), so those follow the move.
    pub fn reorder(&mut self, entry: Id, before: Option<Id>) {
        let Some(list) = self.doc.parent(entry) else {
            return;
        };
        self.checkpoint();
        let old = self.list_of(entry).map(|l| (l, self.entries(l)));
        self.doc.move_to(entry, list, before);
        if let Some((list, old)) = old {
            self.follow_places(list, &old);
        }
        self.rebuild();
    }

    /// Adds an entry to `list` that places `file` (a prop's cfg, a
    /// particle's rdp): a copy of the list's first entry with its file
    /// swapped, else the list's minimal entry.
    pub fn add(&mut self, list: &'static str, file: &str) -> Result<()> {
        let main = self.main().context("the cfg has no MAIN config")?;
        let file = file.trim().replace('/', "\\");
        anyhow::ensure!(!file.is_empty(), "no file named");
        self.checkpoint();
        let list_id = match self.doc.child(main, list) {
            Some(l) => l,
            None => self.doc.insert(main, None, &format!("<{list}></{list}>")),
        };
        let entry = match self.entries(list).first() {
            Some(&first) => {
                let copy = self.doc.duplicate(first);
                self.doc.move_to(copy, list_id, None);
                copy
            }
            None => {
                let kind = match list {
                    "Files" => "FILE",
                    "Particles" => "PARTICLE",
                    "Effects" => "EFFECT",
                    "Lights" => "LIGHT",
                    _ => anyhow::bail!("{list} entries are added by duplicating one"),
                };
                let xml = format!("<Config><ConfigType>{kind}</ConfigType></Config>");
                self.doc.insert(list_id, None, &xml)
            }
        };
        if list != "Lights" {
            self.doc.set_child_text(entry, "FileName", &file);
        }
        if let Some(name) = self.doc.child(entry, "Name") {
            let stem = file.rsplit('\\').next().unwrap_or(&file);
            let stem = stem.split('.').next().unwrap_or(stem).to_owned();
            self.doc.set_text(name, &stem);
        }
        cfg::set_placement(&mut self.doc, entry, &Transform::IDENTITY);
        self.tree.expand(hierarchy::Row::List(list_id));
        self.rebuild();
        self.select(Some(entry));
        Ok(())
    }

    /// Writes the doc into the project, under the cfg's own path, so the
    /// mod replaces the game's model.
    pub fn stage(&mut self, project: &mut crate::project::Project) -> Result<()> {
        anyhow::ensure!(
            !self.path.ends_with(".brp"),
            "a .brp is the game's baked prop cache; edit its .prp"
        );
        self.stage_effects(project)?;
        self.stage_animations(project)?;
        project.stage(&self.path, self.doc.write());
        self.history.mark_saved();
        self.in_project = true;
        Ok(())
    }

    /// The field typing goes to, for the platform's text input.
    pub(crate) fn typing_buffer(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        self.typing
            .as_mut()
            .map(|t| t as &mut dyn gpui_editor_kit::field::InputBuffer)
    }

    /// Where the gizmo sits: the selected entry's origin in model space.
    pub fn pivot(&self) -> Option<(Id, Mat4)> {
        let entry = self.selected?;
        Some((entry, self.placement(entry)?.matrix()))
    }
}

impl Editor {
    /// Opens `path` (a `.cfg` or `.prp`) in the Model workspace: the
    /// project's copy when it has one, else the game's. Edits to the open
    /// one ask first, as every replaced document does (`Editor::leave`).
    pub fn open_model(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.require_workspace(Workspace::Model) {
            return;
        }
        self.leave(crate::editor::Leave::Model(path.to_owned()), cx);
    }

    /// Loads the model at `path` off the UI thread; the open one stays
    /// until it is in, so a failed load keeps it. `discarded`: see
    /// `Editor::go`.
    pub(crate) fn load_model(&mut self, path: &str, discarded: bool, cx: &mut Context<Self>) {
        let Some(install) = self.install.clone() else {
            return self.message("The game is still opening", false);
        };
        self.load_model_sounds(cx);
        let look = self.module.model_look();
        let (module, game) = (self.module, self.module.game());
        let copy = self.project.read_file(path).map(|b| b.into_owned());
        let path = path.to_owned();
        self.message(format!("Opening {path}…"), false);
        self.switch(Workspace::Model);
        cx.spawn(async move |this, cx| {
            let job = install.clone();
            let read = path.clone();
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let skies = sky::offered(module, &job, &read);
                    Loaded::read(&job, &look, game, &read, copy).map(|l| (l, skies))
                })
                .await;
            this.update(cx, |this, cx| {
                // One edited while this loaded, not by a choice to discard:
                // ask again instead of dropping it.
                if loaded.is_ok()
                    && !discarded
                    && this.model.as_ref().is_some_and(ModelEditor::dirty)
                {
                    this.leaving = Some(crate::editor::Leave::Model(path.clone()));
                    cx.notify();
                    return;
                }
                match loaded {
                    Ok((loaded, skies)) => {
                        let note = format!("Opened {}", loaded.path);
                        this.remember_model(&loaded.path);
                        let mut m = ModelEditor::new(install, &look, loaded);
                        m.emitters = this.project.effects.clone();
                        m.sounds = this.model_sound.library();
                        m.view.skies = skies;
                        this.model = Some(m);
                        this.message(note, false);
                        this.load_nested(cx);
                    }
                    Err(e) => this.message(format!("Model not opened: {e:#}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn remember_model(&mut self, path: &str) {
        self.model_recent.retain(|p| p != path);
        self.model_recent.insert(0, path.to_owned());
        self.model_recent.truncate(RECENT_MODELS);
    }

    /// The model open before this one, which Back returns to.
    pub fn previous_model(&self) -> Option<&str> {
        self.model_recent.get(1).map(String::as_str)
    }

    /// Back (Alt Left): the model open before this one, as after Open prop.
    pub fn model_back(&mut self, cx: &mut Context<Self>) {
        match self.previous_model().map(str::to_owned) {
            Some(path) => self.open_model(&path, cx),
            None => self.message("No model was open before this one", false),
        }
    }

    /// After an edit in the Model workspace: its panes redraw, and props
    /// the edit brought in start loading.
    pub(crate) fn model_edited(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = &mut self.model
            && let Some((text, error)) = m.message.take()
        {
            self.message(text, error);
        }
        self.load_nested(cx);
        cx.notify();
    }

    /// Starts loading the open model's queued props on the background
    /// executor, [`NESTED_LOADS`] at a time; each one landing starts the
    /// next. A load for a model since closed is dropped.
    pub(crate) fn load_nested(&mut self, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        for key in m.start_nested() {
            let (install, cache, path) = (m.install.clone(), m.cache.clone(), m.path.clone());
            cx.spawn(async move |this, cx| {
                let (cfg, deform, follow) = key.clone();
                let pieces = cx
                    .background_executor()
                    .spawn(async move { models::nested(&install, &cache, &cfg, deform, follow) })
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(m) = &mut this.model
                        && m.path == path
                    {
                        m.nested_loaded(key, pieces);
                    }
                    this.load_nested(cx);
                    this.notify_pane(crate::editor::Pane::ModelView, cx);
                })
                .ok();
            })
            .detach();
        }
    }
}
