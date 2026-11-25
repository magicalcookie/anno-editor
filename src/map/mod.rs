// Owns the open map layout in the Map workspace: the document with its
// selection, undo ring and the slot edits the tools make, opening shipped
// and project layouts, and saving one into the project. The canvas and its
// tools are in `view`; the side panes and the dialogs in `panels`.

pub mod panels;
pub mod view;

use crate::editor::{Editor, Workspace};
use anno_core::island::{Level, Names, Problem};
use anno_core::map::{
    IslandFacts, LayoutEntry, Map, MapRules, Maps, PlaysAs, Registers, Role, Slot, item_slot,
    slot_item,
};
use anno_core::project::MapEntry;
use anno_formats::vfs::Install;
use anyhow::{Context as _, Result};
use gpui_editor_kit::gpui::{prelude::*, *};
use gpui_editor_kit::history::History;
use std::sync::Arc;

/// Whole-map snapshots undo keeps. A map is 16 bytes a slot plus
/// its strings, and the variants' source bytes are shared, so 256 cost
/// next to nothing.
const UNDO: usize = 256;

/// Pixels around a marker's point a press takes it within.
const MARKER_REACH: f32 = 8.;

/// The Map workspace's tools; keys as the island editor's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tool {
    /// V: select, drag to move.
    #[default]
    Select,
    /// A: click to place a pool slot of the toolbar's role and class.
    Add,
    /// S: place and move free start points (2070's arks).
    Starts,
}

/// A set of slot indices of one variant: one bit each, up to
/// `Map::MAX_SLOTS`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection(Vec<u64>);

impl Selection {
    pub fn has(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    pub fn set(&mut self, i: usize, on: bool) {
        if self.0.len() <= i / 64 {
            self.0.resize(i / 64 + 1, 0);
        }
        let bit = 1 << (i % 64);
        if on {
            self.0[i / 64] |= bit;
        } else {
            self.0[i / 64] &= !bit;
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    pub fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (self.0.iter().enumerate()).flat_map(|(n, &w)| {
            (0..64)
                .filter(move |b| w >> b & 1 == 1)
                .map(move |b| n * 64 + b)
        })
    }
}

/// What a press on the canvas is dragging.
#[derive(Clone, Debug)]
pub enum Drag {
    /// Selected slots, from where they were when the press began, by slot
    /// index; and the press point in metres.
    Slots {
        from: [f32; 2],
        start: Vec<(usize, [i32; 2])>,
        moved: bool,
    },
    /// Free start point `n`; `moved` once it left where it was.
    Start {
        n: usize,
        from: [f32; 2],
        at: [i32; 2],
        moved: bool,
    },
    /// The view, by the right or middle button: the press point in pixels.
    Pan { from: [f32; 2], centre: [f32; 2] },
}

/// Where the canvas looks: the map point at its centre, and pixels per
/// metre. Fitted to the map on the first draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub centre: [f32; 2],
    pub scale: f32,
    pub fitted: bool,
}

/// How a shipped map saves, asked once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveAs {
    /// A project copy under its own name, registered as a layout.
    NewLayout,
    /// The project writes the shipped paths, which the mod layer replaces.
    Override,
}

/// An island's top view, turned 0 to 3 quarter turns, made off the UI
/// thread once per island the map names.
#[derive(Clone, Default)]
pub struct Top {
    pub turns: [Option<Arc<RenderImage>>; 4],
    pub asked: bool,
}

/// The open map layout and what the Map workspace does to it.
pub struct MapEditor {
    pub map: Map,
    /// Where it was read from: a shipped map or a project layout. None for
    /// one New map made that has not been saved.
    pub path: Option<String>,
    /// Read from the install, not a project layout: saving asks how.
    pub shipped: bool,
    pub save_as: Option<SaveAs>,
    /// The variant shown and edited.
    pub variant: usize,
    pub selected: Selection,
    /// The free start point selected, with no slot.
    pub start: Option<usize>,
    pub tool: Tool,
    /// What Add places.
    pub add_role: Role,
    pub add_class: u8,
    /// Moves, turns, roles and deletes also apply to the slots that came
    /// from the same element in the other variants. Off by default:
    /// shipped `_easy/_medium/_hard` files are separate layouts whose
    /// element indices only sometimes line up.
    pub all_variants: bool,
    /// Undo and the saved state: undoing back to the save is clean.
    history: History<Map>,
    pub camera: Camera,
    /// The canvas as last painted, window pixels: `x y w h`.
    pub rect: [f32; 4],
    pub drag: Option<Drag>,
    /// The checks' result for the map as it stands; each problem's
    /// `names` says whether its items are slots, variants or the setups
    /// the map lacks.
    pub problems: Vec<Problem>,
    /// Per variant, the shipped map assets it may play as
    /// (`Maps::plays_as`), listed once assets.xml is in. Region and setups
    /// never change while the map is open.
    pub plays_as: Option<Vec<Vec<PlaysAs>>>,
    /// Per `Map::islands` entry: what the checks and the canvas know of
    /// the island, once looked up, and its top view.
    pub facts: Vec<Option<IslandFacts>>,
    /// `facts` were looked up with the island list loaded: before it is,
    /// shipped islands have no side yet.
    pub facts_listed: bool,
    pub tops: Vec<Top>,
    pub message: Option<(String, bool)>,
    /// The inspector's X or Z while typed into, by axis.
    pub typing: Option<gpui_editor_kit::field::TextEdit<usize>>,
}

impl MapEditor {
    /// `map` as read from `path`, or with no path a new layout: that one
    /// counts as unsaved until a save, whatever undo does.
    pub fn new(map: Map, path: Option<String>, shipped: bool) -> Self {
        let n = map.islands.len();
        let saved = path.is_some();
        Self {
            map,
            path,
            shipped,
            save_as: None,
            variant: 0,
            selected: Selection::default(),
            start: None,
            tool: Tool::Select,
            add_role: Role::Normal,
            add_class: 1,
            all_variants: false,
            history: History::with_depth(saved, UNDO),
            camera: Camera {
                centre: [0., 0.],
                scale: 1.,
                fitted: false,
            },
            rect: [0.; 4],
            drag: None,
            problems: Vec::new(),
            plays_as: None,
            facts: vec![None; n],
            facts_listed: false,
            tops: vec![Top::default(); n],
            message: None,
            typing: None,
        }
    }

    pub fn dirty(&self) -> bool {
        self.history.dirty()
    }

    /// The stem its files take: the name made file-safe.
    pub fn file_name(&self) -> String {
        crate::island::file_name(&self.map.name)
    }

    pub fn slots(&self) -> &[Slot] {
        self.map
            .variants
            .get(self.variant)
            .map_or(&[], |v| v.slots.as_slice())
    }

    /// The shown variant's size, metres: 1800's world sizes differ.
    pub fn size(&self) -> [u32; 2] {
        self.map
            .variants
            .get(self.variant)
            .map_or(self.map.size, |v| v.size)
    }

    pub fn playable(&self) -> [i32; 4] {
        (self.map.variants.get(self.variant)).map_or(self.map.playable, |v| v.playable)
    }

    pub fn starts(&self) -> &[[i32; 2]] {
        self.map
            .variants
            .get(self.variant)
            .map_or(&[], |v| v.starts.as_slice())
    }

    /// Records the map before an edit, for undo.
    fn snapshot(&mut self) {
        self.history.checkpoint(&self.map);
    }

    pub fn undo(&mut self, redo: bool) -> bool {
        let done = if redo {
            self.history.redo(&mut self.map)
        } else {
            self.history.undo(&mut self.map)
        };
        if done {
            self.after_swap();
        }
        done
    }

    /// After undo or redo replaced the map: indices may have moved.
    fn after_swap(&mut self) {
        self.variant = self.variant.min(self.map.variants.len().saturating_sub(1));
        let n = self.slots().len();
        let kept: Vec<usize> = self.selected.iter().filter(|&i| i < n).collect();
        self.selected.clear();
        for i in kept {
            self.selected.set(i, true);
        }
        self.start = self.start.filter(|&s| s < self.starts().len());
        self.grow_islands();
    }

    /// Keeps the per-island tables as long as `Map::islands`.
    fn grow_islands(&mut self) {
        let n = self.map.islands.len();
        self.facts.resize(n, None);
        self.tops.resize(n, Top::default());
    }

    /// Metres a side of the slot's footprint: a fixed island's own side
    /// once known, else its class's slot. Classes past the game's largest
    /// (a template's XL in 1800) take the largest.
    pub fn side(&self, slot: &Slot, rules: &MapRules) -> u32 {
        let classes = rules.size_classes;
        let class = (classes.get(slot.class as usize).or(classes.last())).map_or(256, |c| c.slot);
        if slot.role == Role::Marker {
            // A bare position: it places no island.
            return 0;
        }
        match self.facts.get(slot.island as usize) {
            Some(Some(f)) if slot.fixed() && f.side > 0 => f.side,
            _ => class,
        }
    }

    /// `x0 y0 x1 y1`, metres.
    pub fn footprint(&self, slot: &Slot, rules: &MapRules) -> [i32; 4] {
        let s = self.side(slot, rules) as i32;
        [slot.at[0], slot.at[1], slot.at[0] + s, slot.at[1] + s]
    }

    /// The topmost slot under map point `at`: the last drawn.
    pub fn hit(&self, at: [f32; 2], rules: &MapRules) -> Option<usize> {
        // Markers first: points drawn over the slots around them.
        let reach = MARKER_REACH / self.camera.scale.max(0.01);
        let marker = (0..self.slots().len()).rev().find(|&i| {
            let s = &self.slots()[i];
            s.role == Role::Marker
                && (s.at[0] as f32 - at[0]).abs() <= reach
                && (s.at[1] as f32 - at[1]).abs() <= reach
        });
        marker.or_else(|| self.hit_area(at, rules))
    }

    fn hit_area(&self, at: [f32; 2], rules: &MapRules) -> Option<usize> {
        (0..self.slots().len()).rev().find(|&i| {
            let [x0, y0, x1, y1] = self.footprint(&self.slots()[i], rules).map(|v| v as f32);
            (x0..x1).contains(&at[0]) && (y0..y1).contains(&at[1])
        })
    }

    /// The free start point within `reach` metres of `at`.
    pub fn hit_start(&self, at: [f32; 2], reach: f32) -> Option<usize> {
        (self.starts().iter()).position(|s| {
            (s[0] as f32 - at[0]).abs() <= reach && (s[1] as f32 - at[1]).abs() <= reach
        })
    }

    /// A press on slot `hit` (or on sea): selects it alone, or with `add`
    /// toggles it in the selection.
    pub fn select(&mut self, hit: Option<usize>, add: bool) {
        self.start = None;
        match hit {
            Some(i) if add => {
                let on = !self.selected.has(i);
                self.selected.set(i, on);
            }
            Some(i) if self.selected.has(i) => {}
            Some(i) => {
                self.selected.clear();
                self.selected.set(i, true);
            }
            None if !add => self.selected.clear(),
            None => {}
        }
    }

    /// Starts dragging the selection from map point `from`.
    pub fn begin_move(&mut self, from: [f32; 2]) {
        let start = (self.selected.iter())
            .filter_map(|i| Some((i, self.slots().get(i)?.at)))
            .collect();
        self.drag = Some(Drag::Slots {
            from,
            start,
            moved: false,
        });
    }

    /// The drag at map point `to`: the selection follows, each slot's
    /// corner snapped to `grid`.
    pub fn drag_to(&mut self, to: [f32; 2], grid: u32) {
        match self.drag.clone() {
            Some(Drag::Slots { from, start, moved }) => {
                let delta = [to[0] - from[0], to[1] - from[1]];
                if !moved && delta[0].abs().max(delta[1].abs()) < grid as f32 / 2. {
                    return;
                }
                if !moved {
                    self.snapshot();
                    self.drag = Some(Drag::Slots {
                        from,
                        start: start.clone(),
                        moved: true,
                    });
                }
                let moves: Vec<(usize, [i32; 2])> = (start.iter())
                    .map(|&(i, at)| {
                        let to = [0, 1].map(|k| snap(at[k] as f32 + delta[k], grid));
                        (i, to)
                    })
                    .collect();
                self.apply(&moves.iter().map(|m| m.0).collect::<Vec<_>>(), |slot, n| {
                    slot.at = moves[n].1;
                });
            }
            Some(Drag::Start { n, from, at, moved }) => {
                let to = [0, 1].map(|k| snap(at[k] as f32 + to[k] - from[k], grid));
                if self.starts().get(n).is_none_or(|s| *s == to) {
                    return;
                }
                // One undo step a drag, and none for a press that never
                // moved.
                if !moved {
                    self.snapshot();
                    self.drag = Some(Drag::Start {
                        n,
                        from,
                        at,
                        moved: true,
                    });
                }
                self.map.variants[self.variant].starts[n] = to;
            }
            _ => {}
        }
    }

    /// Applies `edit` to slots `which` of the shown variant, and with
    /// `all_variants` to the slots of the others that came from the same
    /// element. `edit` gets the slot and its position in `which`.
    fn apply(&mut self, which: &[usize], edit: impl Fn(&mut Slot, usize)) {
        let v = self.variant;
        let sources: Vec<u16> = (which.iter())
            .map(|&i| {
                self.map.variants[v]
                    .slots
                    .get(i)
                    .map_or(Slot::NEW, |s| s.source)
            })
            .collect();
        for (n, &i) in which.iter().enumerate() {
            if let Some(slot) = self.map.variants[v].slots.get_mut(i) {
                edit(slot, n);
            }
        }
        if !self.all_variants {
            return;
        }
        for (k, variant) in self.map.variants.iter_mut().enumerate() {
            if k == v {
                continue;
            }
            for slot in &mut variant.slots {
                if slot.source == Slot::NEW {
                    continue;
                }
                if let Some(n) = sources.iter().position(|&s| s == slot.source) {
                    edit(slot, n);
                }
            }
        }
    }

    /// A pool slot of `role` and `class` centred on map point `at`. None
    /// when the variant is full.
    pub fn add_slot(&mut self, at: [f32; 2], rules: &MapRules) -> Option<usize> {
        let classes = rules.size_classes;
        if self.slots().len() >= Map::MAX_SLOTS {
            self.message = Some((
                format!("A variant holds at most {} slots", Map::MAX_SLOTS),
                true,
            ));
            return None;
        }
        let class = self.add_class.min(classes.len().saturating_sub(1) as u8);
        let side = classes.get(class as usize).map_or(256, |c| c.slot) as f32;
        let slot = Slot {
            at: [0, 1].map(|k| snap(at[k] - side / 2., rules.grid)),
            island: Slot::POOL,
            role: self.add_role,
            class,
            turns: 0,
            raw_type: rules.raw_type(self.add_role).unwrap_or(0),
            source: Slot::NEW,
        };
        Some(self.push_slot(slot))
    }

    fn push_slot(&mut self, slot: Slot) -> usize {
        self.snapshot();
        let slots = &mut self.map.variants[self.variant].slots;
        slots.push(slot);
        let i = slots.len() - 1;
        self.selected.clear();
        self.selected.set(i, true);
        self.start = None;
        i
    }

    /// Island `path` dropped on map point `at`: onto the slot there, else
    /// as a new fixed slot centred on it. `facts` sizes it and picks the
    /// class.
    pub fn drop_island(
        &mut self,
        at: [f32; 2],
        path: &str,
        facts: Option<IslandFacts>,
        rules: &MapRules,
    ) -> Option<usize> {
        if let Some(i) = self.hit(at, rules) {
            self.assign(&[i], path, facts, rules);
            return Some(i);
        }
        if self.slots().len() >= Map::MAX_SLOTS {
            self.message = Some((
                format!("A variant holds at most {} slots", Map::MAX_SLOTS),
                true,
            ));
            return None;
        }
        let side = facts.map_or(256, |f| f.side) as f32;
        let role = match facts {
            Some(f) if f.underwater => Role::Underwater,
            Some(f) if f.start && !f.regular => Role::Start,
            _ => Role::Normal,
        };
        let role = if rules.raw_type(role).is_some() {
            role
        } else {
            Role::Normal
        };
        let island = self.map.island(path);
        self.grow_islands();
        if let Some(f) = facts {
            self.facts[island as usize] = Some(f);
        }
        let slot = Slot {
            at: [0, 1].map(|k| snap(at[k] - side / 2., rules.grid)),
            island,
            role,
            class: rules.class_for(side as u32),
            turns: 0,
            raw_type: rules.raw_type(role).unwrap_or(0),
            source: Slot::NEW,
        };
        Some(self.push_slot(slot))
    }

    /// Puts island `path` in slots `which`, making pool slots fixed.
    pub fn assign(
        &mut self,
        which: &[usize],
        path: &str,
        facts: Option<IslandFacts>,
        rules: &MapRules,
    ) {
        if which.is_empty() {
            return;
        }
        self.snapshot();
        let island = self.map.island(path);
        self.grow_islands();
        if let Some(f) = facts {
            self.facts[island as usize] = Some(f);
        }
        let class = facts.map(|f| rules.class_for(f.side));
        self.apply(which, |slot, _| {
            slot.island = island;
            if let Some(c) = class {
                slot.class = c;
            }
        });
    }

    /// Back to pool: the selected slots stop naming an island and the game
    /// fills them again. Not in games without pool slots.
    pub fn back_to_pool(&mut self, rules: &MapRules) {
        if !rules.pool_slots {
            self.message = Some((
                "This game's maps have no pool slots: every slot names its island".into(),
                true,
            ));
            return;
        }
        let which = self.fixed_selected();
        if which.is_empty() {
            return;
        }
        self.snapshot();
        self.apply(&which, |slot, _| {
            slot.island = Slot::POOL;
            slot.turns = 0;
        });
    }

    fn fixed_selected(&self) -> Vec<usize> {
        (self.selected.iter())
            .filter(|&i| self.slots().get(i).is_some_and(|s| s.fixed()))
            .collect()
    }

    pub fn delete_selected(&mut self) -> usize {
        if let Some(n) = self.start.take() {
            self.snapshot();
            self.map.variants[self.variant].starts.remove(n);
            return 1;
        }
        let gone = self.selected.clone();
        let count = gone.count();
        if count == 0 {
            return 0;
        }
        self.snapshot();
        let v = self.variant;
        let sources: Vec<u16> = (gone.iter())
            .filter_map(|i| self.map.variants[v].slots.get(i).map(|s| s.source))
            .filter(|s| *s != Slot::NEW)
            .collect();
        let mut i = 0;
        self.map.variants[v].slots.retain(|_| {
            let keep = !gone.has(i);
            i += 1;
            keep
        });
        if self.all_variants {
            for (k, variant) in self.map.variants.iter_mut().enumerate() {
                if k != v {
                    variant.slots.retain(|s| !sources.contains(&s.source));
                }
            }
        }
        self.selected.clear();
        count
    }

    /// A quarter turn of the selected fixed islands, where the game turns
    /// them. Clockwise on the canvas; which way `Rotation90` turns in game
    /// is the in-game check's.
    pub fn rotate_selected(&mut self, rules: &MapRules) {
        if !rules.fixed_turns {
            self.message = Some(("This game's maps don't turn islands".into(), true));
            return;
        }
        let which = self.fixed_selected();
        if which.is_empty() {
            self.message = Some((
                "Only slots with an island turn; pool slots take the game's".into(),
                false,
            ));
            return;
        }
        self.snapshot();
        self.apply(&which, |slot, _| slot.turns = (slot.turns + 1) % 4);
    }

    pub fn set_role(&mut self, role: Role, rules: &MapRules) {
        let Some(raw) = rules.raw_type(role) else {
            return;
        };
        let which: Vec<usize> = self.selected.iter().collect();
        if which.is_empty() {
            self.add_role = role;
            return;
        }
        self.snapshot();
        self.apply(&which, |slot, _| {
            slot.role = role;
            slot.raw_type = raw;
        });
    }

    /// One size class smaller (`-1`) or larger, for the selection, or for
    /// what Add places when nothing is selected.
    pub fn step_class(&mut self, by: i32, rules: &MapRules) {
        let top = rules.size_classes.len().saturating_sub(1) as i32;
        let step = |c: u8| (c as i32 + by).clamp(0, top) as u8;
        let which: Vec<usize> = self.selected.iter().collect();
        if which.is_empty() {
            self.add_class = step(self.add_class);
            return;
        }
        self.snapshot();
        self.apply(&which, |slot, _| slot.class = step(slot.class));
    }

    /// Class `c` for the selection, or for what Add places when nothing is
    /// selected.
    pub fn set_class(&mut self, c: u8) {
        let which: Vec<usize> = self.selected.iter().collect();
        if which.is_empty() {
            self.add_class = c;
            return;
        }
        self.snapshot();
        self.apply(&which, |slot, _| slot.class = c);
    }

    /// Sets the selected slots' corner, metres, on one axis.
    pub fn set_position(&mut self, axis: usize, value: i32) {
        let which: Vec<usize> = self.selected.iter().collect();
        // The value the field showed, entered again, is no edit.
        let slots = self.slots();
        if which
            .iter()
            .all(|&i| slots.get(i).is_none_or(|s| s.at[axis] == value))
        {
            return;
        }
        self.snapshot();
        self.apply(&which, |slot, _| slot.at[axis] = value);
    }

    /// A free start point at map point `at`.
    pub fn add_start(&mut self, at: [f32; 2], grid: u32) {
        self.snapshot();
        let p = [0, 1].map(|k| snap(at[k], grid));
        let starts = &mut self.map.variants[self.variant].starts;
        starts.push(p);
        self.start = Some(starts.len() - 1);
        self.selected.clear();
    }

    pub fn begin_start_move(&mut self, n: usize, from: [f32; 2]) {
        self.start = Some(n);
        self.selected.clear();
        let at = self.starts()[n];
        self.drag = Some(Drag::Start {
            n,
            from,
            at,
            moved: false,
        });
    }

    /// Copies the shown variant's slots and starts to every other variant.
    /// The copies are new elements there: a variant's element indices are
    /// its own file's, so a copy carries no per-slot data the target had
    /// (its difficulty ids come from an element of the same kind).
    pub fn copy_to_other_variants(&mut self) {
        if self.map.variants.len() < 2 {
            return;
        }
        self.snapshot();
        let v = self.variant;
        let slots: Vec<Slot> = (self.map.variants[v].slots.iter())
            .map(|s| Slot {
                source: Slot::NEW,
                ..*s
            })
            .collect();
        let starts = self.map.variants[v].starts.clone();
        for (k, variant) in self.map.variants.iter_mut().enumerate() {
            if k != v {
                variant.slots = slots.clone();
                variant.starts = starts.clone();
            }
        }
    }

    pub fn show_variant(&mut self, v: usize) {
        if v < self.map.variants.len() && v != self.variant {
            // Another world size fits the view to itself.
            let before = self.size();
            self.variant = v;
            self.camera.fitted &= self.size() == before;
            self.selected.clear();
            self.start = None;
            self.drag = None;
        }
    }

    /// The problems naming slot `i` of the shown variant.
    pub fn problems_of(&self, i: usize) -> impl Iterator<Item = &Problem> {
        let item = slot_item(self.variant, i);
        (self.problems.iter())
            .filter(|p| p.names == Names::Slots)
            .filter(move |p| p.items.contains(&item))
    }

    /// Whether a problem names variant `v` or one of its slots: its tab
    /// says so, so a problem in a variant not shown is found.
    pub fn variant_has_problems(&self, v: usize) -> bool {
        self.problems.iter().any(|p| match p.names {
            Names::Slots => p.items.iter().any(|&i| item_slot(i).0 == v),
            Names::Variants => p.items.contains(&(v as u64)),
            Names::Setups | Names::Objects => false,
        })
    }

    /// Shows what problem `n` names: its slots selected, in the shown
    /// variant where it names any there, else in the first it names; a
    /// variant shown, with nothing selected; for the whole map, nothing
    /// selected, so the inspector shows the map.
    pub fn select_problem(&mut self, n: usize) {
        let Some(p) = self.problems.get(n) else {
            return;
        };
        let (names, items) = (p.names, p.items.clone());
        let variants: Vec<usize> = match names {
            Names::Slots => items.iter().map(|&i| item_slot(i).0).collect(),
            Names::Variants => items.iter().map(|&v| v as usize).collect(),
            Names::Setups | Names::Objects => Vec::new(),
        };
        if !variants.contains(&self.variant)
            && let Some(&v) = variants.first()
        {
            self.show_variant(v);
        }
        self.selected.clear();
        self.start = None;
        if names == Names::Slots {
            let n = self.slots().len();
            for (v, s) in items.iter().map(|&i| item_slot(i)) {
                if v == self.variant && s < n {
                    self.selected.set(s, true);
                }
            }
        }
    }

    /// Problem `p` in words with what it names where that isn't a slot of
    /// the shown variant: "Variants with too few starts · Small islands",
    /// "Setup options no variant answers · the whole map lacks Large ·
    /// Small islands".
    pub fn problem_words(&self, p: &Problem, rules: &MapRules) -> String {
        let all = &self.map.variants;
        let mut variants: Vec<usize> = match p.names {
            Names::Slots => p.items.iter().map(|&i| item_slot(i).0).collect(),
            Names::Variants => p.items.iter().map(|&v| v as usize).collect(),
            Names::Setups => {
                let worlds = rules
                    .setups
                    .iter()
                    .any(|s| s.world_size != rules.setups[0].world_size);
                let lacks: Vec<String> = (p.items.iter())
                    .filter_map(|&n| rules.setups.get(n as usize))
                    .map(|s| view::setup_label(*s, worlds))
                    .collect();
                return format!("{} · the whole map lacks {}", p.what, lacks.join(", "));
            }
            Names::Objects => return p.said(),
        };
        variants.sort_unstable();
        variants.dedup();
        // One variant's slots in the one shown need no naming.
        if all.len() < 2 || (p.names == Names::Slots && variants == [self.variant]) {
            return p.said();
        }
        let named: Vec<String> = (variants.iter())
            .filter_map(|&v| all.get(v))
            .map(|v| view::variant_label(all, v))
            .collect();
        format!("{} · {}", p.said(), named.join(", "))
    }

    /// Makes the shown variant play as shipped map asset `guid`, or stop.
    pub fn set_plays_as(&mut self, guid: u32, on: bool) {
        let Some(v) = self.map.variants.get(self.variant) else {
            return;
        };
        if v.replaces.contains(&guid) == on {
            return;
        }
        self.snapshot();
        let replaces = &mut self.map.variants[self.variant].replaces;
        if on {
            replaces.push(guid);
            replaces.sort_unstable();
        } else {
            replaces.retain(|g| *g != guid);
        }
    }

    /// Runs the game's checks on the map as it stands.
    pub fn check(&mut self, maps: &dyn Maps, refs: &anno_core::game::Refs) {
        let facts = |path: &str| {
            let i = self.map.islands.iter().position(|p| p == path)?;
            self.facts.get(i).copied().flatten()
        };
        self.problems = maps.validate(&self.map, refs, &facts);
        // Level 5 first: it blocks; the rest (a setup without a variant)
        // only warns.
        self.problems.sort_by_key(|p| p.level != Level::Broken);
    }

    /// The level-5 problems: what stops the map leaving the editor.
    pub fn blocking(&self) -> impl Iterator<Item = &Problem> {
        self.problems.iter().filter(|p| p.level == Level::Broken)
    }

    /// The name to show: a shipped map's is its file stem, so in words.
    pub fn title(&self) -> String {
        match self.shipped {
            true => panels::pretty_words(&self.map.name),
            false => self.map.name.clone(),
        }
    }

    /// `path` opens this map: the path it was read from (as `.a7t`), or
    /// for a shipped map one of its variants' files.
    pub fn is_at(&self, path: &str) -> bool {
        let path = canonical_map_path(path);
        self.path.as_deref() == Some(path.as_str())
            || (self.shipped
                && self
                    .map
                    .variants
                    .iter()
                    .any(|v| format!("{}.a7t", v.base) == path))
    }

    pub fn saved(&mut self, path: String) {
        self.history.mark_saved();
        self.shipped = false;
        self.path = Some(path);
    }
}

/// `v` metres snapped to the nearest multiple of `grid`.
pub fn snap(v: f32, grid: u32) -> i32 {
    let g = grid.max(1) as f32;
    ((v / g).round() * g) as i32
}

/// What `path` is to the checks and the canvas: its side from the island
/// list, its roles from the project's listing or the list's kind.
pub fn island_facts(editor: &Editor, path: &str) -> Option<IslandFacts> {
    let rules = editor.module.islands()?.rules();
    let listed = (editor.island_list.as_deref())
        .and_then(|l| l.iter().find(|s| s.path.eq_ignore_ascii_case(path)));
    let project = editor.project.islands.iter().find(|i| i.path == path);
    let side = listed.map(|s| s.world[0]).or_else(|| {
        // A project island not in the list yet (made this session): its
        // size class's slot stands in.
        let p = project?;
        rules
            .size_classes
            .iter()
            .find(|c| c.name == p.class())
            .map(|c| c.slot)
    })?;
    let (regular, start, broken) = match project {
        Some(p) => (
            p.listing.is_regular(rules),
            p.listing.is_start(rules),
            !p.listing.broken.is_empty(),
        ),
        None => (
            true,
            rules
                .size_classes
                .iter()
                .any(|c| c.start && side <= c.slot && side > 0),
            false,
        ),
    };
    Some(IslandFacts {
        side,
        regular,
        start,
        underwater: path.contains("/underwater/") || path.contains("/u_"),
        broken,
    })
}

impl Editor {
    /// The game's maps capability, where it has one.
    pub fn maps(&self) -> Option<&'static dyn Maps> {
        self.module.islands()?.maps()
    }

    /// Opens a shipped or project map in the Map workspace, asking first
    /// when the open map has edits (`Editor::leave`).
    pub fn open_map(&mut self, path: &str, cx: &mut Context<Self>) {
        self.leave(crate::editor::Leave::Map(path.to_owned()), cx);
    }

    /// Reads the map at `path` and makes it the open one. One that doesn't
    /// read leaves the open map as it was.
    pub(crate) fn read_map(&mut self, path: &str, cx: &mut Context<Self>) {
        let (Some(maps), Some(install)) = (self.maps(), self.install.clone()) else {
            return self.message("This game's maps don't open in the editor", true);
        };
        let path = canonical_map_path(path);
        let ours = (self.project.layouts.iter())
            .find(|l| l.path == path || l.variants.iter().any(|v| v.path == path));
        // A layout's files name it by their stem; the project keeps the
        // name it was given.
        let read = match ours {
            Some(entry) => read_layout(maps, &install, entry).map(|mut map| {
                map.name = entry.name.clone();
                map
            }),
            None => maps.read(&install, &path),
        };
        let shipped = ours.is_none();
        let path = ours.map_or(path, |l| l.path.clone());
        match read {
            Ok(map) => {
                let name = map.name.clone();
                self.set_map(MapEditor::new(map, Some(path), shipped), cx);
                self.switch(Workspace::Map);
                self.message(format!("Opened {name}"), false);
            }
            Err(e) => self.message(format!("Could not open the map: {e:#}"), true),
        }
    }

    /// Makes `map` the open one: looks up its islands and checks it.
    pub(crate) fn set_map(&mut self, mut map: MapEditor, cx: &mut Context<Self>) {
        for (i, path) in map.map.islands.clone().iter().enumerate() {
            map.facts[i] = island_facts(self, path);
        }
        map.facts_listed = self.island_list.is_some();
        self.map = Some(map);
        self.check_map();
        self.load_tops(cx);
    }

    /// Facts for islands added since the last look, the checks again, and
    /// any new top views. Called after every edit.
    pub(crate) fn map_edited(&mut self, cx: &mut Context<Self>) {
        let missing: Vec<(usize, String)> = (self.map.iter())
            .flat_map(|m| {
                m.map
                    .islands
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| m.facts[*i].is_none())
            })
            .map(|(i, p)| (i, p.clone()))
            .collect();
        for (i, path) in missing {
            let facts = island_facts(self, &path);
            if let Some(m) = &mut self.map {
                m.facts[i] = facts;
            }
        }
        self.check_map();
        self.load_tops(cx);
        if let Some((text, error)) = self.map.as_mut().and_then(|m| m.message.take()) {
            self.message(text, error);
        }
    }

    /// The island list the facts read sides from: loaded with the first
    /// map view, and the facts looked up again once it arrives. It is the
    /// Islands pane's list, which loads the same way when that pane draws.
    pub(crate) fn map_facts_need_list(&mut self, cx: &mut Context<Self>) {
        if self.island_list.is_none() {
            let (Some(install), Some(assets)) = (self.install.clone(), self.assets.clone()) else {
                return;
            };
            if std::mem::replace(&mut self.island_list_loading, true) {
                return;
            }
            let module = self.module;
            cx.spawn(async move |this, cx| {
                let list = cx
                    .background_spawn(async move {
                        crate::island::islands(module, &install, Some(&assets))
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.island_list = Some(Arc::new(list));
                    this.island_list_loading = false;
                    this.map_facts_need_list(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        if self.map.as_ref().is_none_or(|m| m.facts_listed) {
            return;
        }
        let paths = self
            .map
            .as_ref()
            .map(|m| m.map.islands.clone())
            .unwrap_or_default();
        let facts: Vec<_> = paths.iter().map(|p| island_facts(self, p)).collect();
        if let Some(m) = &mut self.map {
            m.facts = facts;
            m.facts_listed = true;
        }
        self.check_map();
    }

    pub(crate) fn check_map(&mut self) {
        let (Some(maps), Some(install)) = (self.maps(), self.install.clone()) else {
            return;
        };
        let project = &self.project;
        let has = |p: &str| project.has_file(p);
        let refs = anno_core::game::Refs {
            install: &install,
            project: &has,
            assets: self.assets.as_deref(),
        };
        if let Some(m) = &mut self.map {
            m.check(maps, &refs);
        }
    }

    /// The project's layouts that can't leave the editor, each as "name:
    /// problems": the open map as it stands, the others as saved. Level 5
    /// blocks Install and Test in game, as an island's does.
    pub(crate) fn broken_maps(&self) -> Vec<String> {
        let (Some(maps), Some(install)) = (self.maps(), self.install.as_deref()) else {
            return Vec::new();
        };
        let said = |name: &str, problems: &[Problem]| {
            let words: Vec<String> = problems.iter().map(Problem::said).collect();
            format!("{name}: {}", words.join(", "))
        };
        let mut out = Vec::new();
        let open = self.map.as_ref().filter(|m| !m.shipped);
        if let Some(m) = open {
            let blocking: Vec<Problem> = m.blocking().cloned().collect();
            if !blocking.is_empty() {
                out.push(said(&m.map.name, &blocking));
            }
        }
        let project = &self.project;
        let has = |p: &str| project.has_file(p);
        let refs = anno_core::game::Refs {
            install,
            project: &has,
            assets: self.assets.as_deref(),
        };
        for l in &self.project.layouts {
            if open.is_some_and(|m| m.path.as_deref() == Some(l.path.as_str())) {
                continue;
            }
            let Ok(map) = read_layout(maps, install, l) else {
                out.push(format!("{}: its files don't read", l.name));
                continue;
            };
            let facts = |p: &str| island_facts(self, p);
            let problems: Vec<Problem> = (maps.validate(&map, &refs, &facts).into_iter())
                .filter(|p| p.level == Level::Broken)
                .collect();
            if !problems.is_empty() {
                out.push(said(&l.name, &problems));
            }
        }
        out
    }

    /// The top views of the open map's fixed islands, made off the UI
    /// thread, each island once.
    fn load_tops(&mut self, cx: &mut Context<Self>) {
        let (Some(map), Some(install)) = (&mut self.map, self.install.clone()) else {
            return;
        };
        let wanted: Vec<(usize, String)> = (map.map.islands.iter().enumerate())
            .filter(|(i, _)| !map.tops[*i].asked)
            .map(|(i, p)| (i, p.clone()))
            .collect();
        if wanted.is_empty() {
            return;
        }
        for (i, _) in &wanted {
            map.tops[*i].asked = true;
        }
        let module = self.module;
        cx.spawn(async move |this, cx| {
            let made = cx
                .background_spawn(async move {
                    (wanted.into_iter())
                        .map(|(i, path)| {
                            (i, path.clone(), view::top_views(module, &install, &path))
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(m) = &mut this.map {
                    for (i, path, turns) in made {
                        if m.map.islands.get(i) == Some(&path) {
                            m.tops[i].turns = turns;
                        }
                    }
                }
                this.notify_pane(crate::editor::Pane::MapView, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Map → Close, once its edits are saved or dropped.
    pub(crate) fn close_map(&mut self) {
        if let Some(m) = self.map.take() {
            self.message(format!("Closed {}", m.title()), false);
        }
    }

    /// Stages the open map into the project, as Ctrl S does for every open
    /// document. Ok(false) when there was nothing to stage or the user must
    /// first choose how a shipped map saves (`Dialog::SaveShipped` open).
    pub(crate) fn stage_map(&mut self) -> Result<bool> {
        let Some(m) = &self.map else {
            return Ok(false);
        };
        if !m.dirty() {
            return Ok(false);
        }
        if m.shipped && m.save_as.is_none() {
            let dialog = self.save_shipped_dialog();
            self.map_dialog = Some(panels::Dialog::SaveShipped(dialog));
            return Ok(false);
        }
        // The warning on save names the map as it is written.
        self.check_map();
        let maps = self.maps().context("This game's maps can't be saved")?;
        let install = self.install.clone().context("No install is open")?;
        let m = self.map.as_ref().expect("checked above");
        let files = match m.save_as {
            Some(SaveAs::Override) => override_files(maps, &install, &m.map)?,
            _ => maps.write(&install, &m.map, &m.file_name())?,
        };
        // An override registers nothing: the shipped assets already name
        // the paths it replaces.
        let path = if m.save_as == Some(SaveAs::Override) {
            let base = m
                .map
                .variants
                .first()
                .map(|v| v.base.clone())
                .unwrap_or_default();
            for (path, bytes) in files {
                self.project.stage(&path, bytes);
            }
            format!("{base}.a7t")
        } else {
            // The last guard against replacing another layout: New and Save
            // as new layout check the same as the name is typed.
            let paths = map_files(&files);
            let own = m.path.as_deref().filter(|_| !m.shipped);
            if let Some(why) = layout_clash(&self.project.layouts, &m.map.name, &paths, own) {
                // Asked again on the next save, under another name.
                if let Some(m) = &mut self.map
                    && m.shipped
                {
                    m.save_as = None;
                }
                anyhow::bail!(why);
            }
            let entry = layout_entry(&m.map, &files, self.project.layouts.iter())
                .with_context(|| format!("{} wrote no map file", m.map.name))?;
            for (path, bytes) in files {
                self.project.stage(&path, bytes);
            }
            let path = entry.path.clone();
            self.project.set_layout(entry, maps.rules().registers);
            path
        };
        let m = self.map.as_mut().expect("checked above");
        m.saved(path);
        let first = m.blocking().next().map(Problem::said);
        if let Some(said) = first {
            m.message = Some((
                format!("The game can't load {} yet: {said}", m.map.name),
                true,
            ));
        }
        Ok(true)
    }

    /// Map → New map layout…'s result: opens unsaved.
    pub(crate) fn create_map(&mut self, cx: &mut Context<Self>) {
        let (Some(maps), Some(install)) = (self.maps(), self.install.clone()) else {
            return;
        };
        let Some(panels::Dialog::New(d)) = &self.map_dialog else {
            return;
        };
        let name = d.name.text.trim().to_owned();
        if name.is_empty() {
            return self.message("The map needs a name", true);
        }
        let made = match &d.from {
            panels::From::Empty => maps.new_map(&install, &d.base, &name),
            panels::From::Copy(path) => (self.project.layouts.iter().find(|l| l.path == *path))
                .map_or_else(
                    || maps.read(&install, path),
                    |l| read_layout(maps, &install, l),
                )
                .map(|mut map| {
                    map.name = name.clone();
                    map
                }),
        };
        // Its files as the game writes them, against every layout's: two
        // names can make one stem ("Astra Straits", "Astra-Straits").
        let made = made.and_then(|map| {
            let files = maps.write(&install, &map, &crate::island::file_name(&name))?;
            match layout_clash(&self.project.layouts, &name, &map_files(&files), None) {
                Some(why) => anyhow::bail!(why),
                None => Ok(map),
            }
        });
        match made {
            Ok(map) => {
                self.map_dialog = None;
                self.set_map(MapEditor::new(map, None, false), cx);
                self.switch(Workspace::Map);
                self.message(
                    format!("New map {name}: Ctrl S saves it into the project"),
                    false,
                );
            }
            Err(e) => self.message(format!("Could not make the map: {e:#}"), true),
        }
    }
}

/// A project layout as saved: what its files hold, and the shipped assets
/// each variant plays as, which only the project's entry records (1800's
/// files don't name them).
pub(crate) fn read_layout(maps: &dyn Maps, install: &Install, entry: &LayoutEntry) -> Result<Map> {
    let mut map = maps.read(install, &entry.path)?;
    for (v, saved) in map.variants.iter_mut().zip(&entry.variants) {
        if v.replaces.is_empty() {
            v.replaces = saved.source_guids.clone();
        }
    }
    Ok(map)
}

/// The map files among a layout's files: each variant's `.a7t`, or the
/// `.www`. A layout's project entry lists these.
pub(crate) fn map_files(files: &[(String, Vec<u8>)]) -> Vec<String> {
    (files.iter())
        .map(|f| f.0.clone())
        .filter(|p| p.ends_with(".a7t") || p.ends_with(".www"))
        .collect()
}

/// Why a layout named `name`, whose map files are `paths`, can't be saved
/// into the project: another layout has the name, or one of its files is
/// at one of the paths and would be replaced. `own` is the open layout's
/// path, which it may replace. The paths are the game's own (`Maps::write`),
/// so two names that make one stem are caught.
pub(crate) fn layout_clash(
    layouts: &[LayoutEntry],
    name: &str,
    paths: &[String],
    own: Option<&str>,
) -> Option<String> {
    let name = name.trim();
    let others = layouts.iter().filter(|l| Some(l.path.as_str()) != own);
    for l in others {
        if l.name.trim().eq_ignore_ascii_case(name) {
            return Some(format!("The project already has a layout named {}", l.name));
        }
        let mut theirs = std::iter::once(&l.path).chain(l.variants.iter().map(|v| &v.path));
        if theirs.any(|t| paths.iter().any(|p| p.eq_ignore_ascii_case(t))) {
            return Some(format!(
                "{name} would save over the layout {}: the two names make the same file name",
                l.name
            ));
        }
    }
    None
}

/// `.a7tinfo` and `.a7te` open as the `.a7t` beside them.
fn canonical_map_path(path: &str) -> String {
    for ext in [".a7tinfo", ".a7te"] {
        if let Some(stem) = path.strip_suffix(ext) {
            return format!("{stem}.a7t");
        }
    }
    path.to_owned()
}

/// The layout's project entry from the files `write` made: each variant's
/// map file (`.a7t`, or the `.www`) in variant order. Keeps the GUIDs and
/// test-map choice of the entry it replaces.
pub(crate) fn layout_entry<'a>(
    map: &Map,
    files: &[(String, Vec<u8>)],
    existing: impl Iterator<Item = &'a LayoutEntry>,
) -> Option<LayoutEntry> {
    let paths: Vec<&str> = (files.iter())
        .map(|f| f.0.as_str())
        .filter(|p| p.ends_with(".a7t") || p.ends_with(".www"))
        .collect();
    let first = paths.first()?.to_string();
    let variants = (map.variants.iter().zip(&paths))
        .map(|(v, p)| MapEntry {
            path: (*p).to_owned(),
            region: map.region.clone(),
            island_size: v.setup.island_size.to_owned(),
            source_guids: v.replaces.clone(),
        })
        .collect();
    // By path only: a layout of the same name elsewhere is another one
    // (`layout_clash` keeps names apart before it gets here).
    let old = existing.into_iter().find(|l| l.path == first);
    Some(LayoutEntry {
        name: map.name.clone(),
        path: first,
        region: map.region.clone(),
        variants,
        guids: old.map(|o| o.guids.clone()).unwrap_or_default(),
        fill_with_islands: old.is_some_and(|o| o.fill_with_islands),
    })
}

/// Override in place: the `.a7tinfo` a project layout would get, at the
/// shipped path each variant was read from, so the mod layer replaces it.
/// The `.a7t` and `.a7te` stay the shipped ones.
fn override_files(maps: &dyn Maps, install: &Install, map: &Map) -> Result<Vec<(String, Vec<u8>)>> {
    anyhow::ensure!(
        maps.rules().registers != Registers::None,
        "this game has no mod delivery, so a shipped map can't be overridden; save it as a new layout"
    );
    let files = maps.write(install, map, &crate::island::file_name(&map.name))?;
    let infos: Vec<Vec<u8>> = (files.into_iter())
        .filter(|f| f.0.ends_with(".a7tinfo"))
        .map(|f| f.1)
        .collect();
    anyhow::ensure!(
        infos.len() == map.variants.len(),
        "{} wrote {} map infos for {} variants",
        map.name,
        infos.len(),
        map.variants.len()
    );
    Ok((map.variants.iter().zip(infos))
        .map(|(v, info)| (format!("{}.a7tinfo", v.base), info))
        .collect())
}

#[cfg(test)]
mod tests;
