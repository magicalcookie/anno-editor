// What a wire or a removal does to a file's bindings: the places slots
// name and who writes and reads them, the kind checks, the verdict a drop
// would get, and the edits themselves (wire, rewire, the remove menu).
use super::{Graph, Kind, gate_pair, owner_title, place_key, plain_copy};
use crate::ui::UiEditor;
use crate::ui::bindings::{
    ASSIGN, FileTable, T_ACTION, T_BOOL, T_EVENT, T_FLOAT, T_OBJECT, T_STRING, Target, in_words,
    kind_name, logic_name, plain, slot_widget,
};
use anno_formats::phx::{
    DefaultValue,
    binding::{Converter, NewBinding, NewSlot, Slot, builtin},
};
use anyhow::{Result, bail};
use gpui_editor_kit::gpui::{Pixels, Point};
use std::collections::HashMap;

/// A place no binding of the file names yet, made when a wire lands on
/// or leaves its `+` pin, as the game's files reach one (round 16: a clip
/// nobody started, and a new button's OnPress, had no pin): a clip
/// action or event (`builtin::START` …) of the file's clip `clip`, or of
/// clip `clip` in the template container `widget` shows, or an event the
/// widget type of container `widget` declares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::ui) enum NewPlace {
    Clip {
        clip: usize,
        id: u32,
    },
    NestedClip {
        widget: usize,
        clip: anno_formats::phx::Guid,
        id: u32,
    },
    Event {
        widget: usize,
        uid: u32,
    },
}

impl NewPlace {
    fn slot(self) -> NewSlot {
        match self {
            NewPlace::Clip { clip, id } => NewSlot::Clip {
                clip,
                path: vec![(1, 0x8000_0000 | id)],
            },
            NewPlace::NestedClip { widget, clip, id } => NewSlot::NestedClip {
                widget,
                clip,
                path: vec![(1, 0x8000_0000 | id)],
            },
            NewPlace::Event { widget, uid } => NewSlot::Widget {
                widget,
                path: vec![(1, uid)],
            },
        }
    }

    /// The place as its pin will read once it is in the file: "clip
    /// Nothing · Start", "BtnOverlay_DS › clip Enter · Start", "Btn ·
    /// OnPress".
    fn label(self, ft: &FileTable) -> String {
        let what = |id: u32| builtin::name(id).unwrap_or("?");
        match self {
            NewPlace::Clip { clip, id } => format!(
                "clip {} · {}",
                ft.asset.clips().get(clip).map_or("?", |c| c.name.as_str()),
                what(id)
            ),
            NewPlace::NestedClip { widget, clip, id } => {
                let name = ft
                    .template(widget)
                    .and_then(|t| {
                        let c = t.clip_ids().iter().position(|c| *c == Some(clip))?;
                        Some(t.clips()[c].name.clone())
                    })
                    .unwrap_or_else(|| "?".into());
                format!("{} › clip {name} · {}", owner_title(ft, widget), what(id))
            }
            NewPlace::Event { widget, uid } => {
                // A built-in (the root's Shown) by its name, else the
                // widget type's event.
                let name = match builtin::name(uid & 0x7fff_ffff).filter(|_| uid & 0x8000_0000 != 0)
                {
                    Some(name) => name.to_owned(),
                    None => ft
                        .type_events(widget)
                        .into_iter()
                        .find(|e| e.0 == uid)
                        .map_or_else(|| format!("{uid:08x}"), |e| e.1),
                };
                format!("{} · {name}", owner_title(ft, widget))
            }
        }
    }

    /// What a wire into it does: "starts", "stops".
    fn verb(self) -> &'static str {
        match self {
            NewPlace::Clip { id, .. } | NewPlace::NestedClip { id, .. } => clip_verb(id),
            NewPlace::Event { .. } => None,
        }
        .unwrap_or("stops")
    }
}

/// What a wire into clip action `id` does to the clip.
fn clip_verb(id: u32) -> Option<&'static str> {
    match id {
        builtin::START => Some("starts"),
        builtin::STOP => Some("stops"),
        _ => None,
    }
}

/// Where a wire starts: a place of the file (a `Bindings::slots` index), or
/// one nothing reads yet that the wire makes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::ui) enum Source {
    Slot(usize),
    New(NewPlace),
}

impl From<usize> for Source {
    fn from(s: usize) -> Source {
        Source::Slot(s)
    }
}

/// The bindings a right-click would remove, and every property they set,
/// shown before anything goes: one converter can feed several.
#[derive(Clone, Debug)]
pub(super) struct UnbindMenu {
    /// Pane pixels.
    pub(super) at: [f32; 2],
    pub(super) bindings: Vec<usize>,
    pub(super) sets: Vec<String>,
    /// Widget properties further along that lose their value with them
    /// (the Icon's Size behind a Multiply that only sets a template
    /// property).
    pub(super) reach: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::ui) enum Drop {
    Slot(usize),
    Open {
        widget: usize,
        target: Target,
    },
    /// A place no binding writes yet (a clip's Start).
    New(NewPlace),
    /// Input `k` of converter binding `binding`: rewires that argument.
    Arg {
        binding: usize,
        k: usize,
    },
    /// A pin that takes no wire, and why: a drop there says so.
    No(&'static str),
}

/// A widget property nothing binds yet as it reads once bound: "Icon ·
/// AlphaTint".
fn open_label(ft: &FileTable, widget: usize, target: Target) -> String {
    let name = ft.widgets.get(widget).and_then(|w| w.name.clone());
    format!("{} · {}", name.unwrap_or_default(), target.label())
}

/// The bindings that write a place (`writes` says which slots are it), as
/// a new binding there would take them over: plain single-target copies,
/// replaced (ascending), or why not. One rule for the drag's verdict,
/// `wire_up` and the binder.
pub(in crate::ui) fn writers(
    table: &anno_formats::phx::binding::Bindings,
    writes: impl Fn(usize) -> bool,
) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for (i, b) in table.bindings.iter().enumerate() {
        if !b.targets.iter().any(|&x| writes(x)) {
            continue;
        }
        if !plain_copy(b) {
            return Err("a converter sets it now; remove that binding first (right-click its node in the graph, or × in Data)".into());
        }
        if b.targets.len() != 1 {
            return Err(
                "the copy that sets it also sets other properties; remove that binding first"
                    .into(),
            );
        }
        out.push(i);
    }
    Ok(out)
}

/// What a wire onto place `t` does to the bindings that write it. An event
/// takes several triggers, as the game's files have them (a press and a
/// gamepad press start one clip in 108 shipped buttons), so they stay; a
/// value has one writer, and the plain copies of it are replaced, or why
/// not (`writers`). Round 16 replaced triggers, two of them while naming
/// one.
enum Takeover {
    Joins(Vec<usize>),
    Replaces(Vec<usize>),
}

fn takeover(ft: &FileTable, places: &Places, t: usize) -> Result<Takeover, String> {
    let at = places.of(t);
    let bindings = &ft.table.bindings;
    if ft.event_slot(t) == Some(true) {
        let ws = (0..bindings.len())
            .filter(|&i| bindings[i].targets.iter().any(|&x| meet(places.of(x), at)))
            .collect();
        return Ok(Takeover::Joins(ws));
    }
    writers(&ft.table, |x| meet(places.of(x), at)).map(Takeover::Replaces)
}

/// Bindings `ws` by what they pass on: a copy by its source, a converter by
/// its node's title and first source ("AnnoButtonWT · OnMouseLeave and
/// when it turns false (AnnoButtonWT · Focus.IsFocused)").
fn writer_names(ft: &FileTable, ws: &[usize]) -> String {
    let names: Vec<String> = ws
        .iter()
        .map(|&i| {
            let b = &ft.table.bindings[i];
            let source = b
                .sources
                .first()
                .map_or_else(String::new, |&s| plain(ft.label(s)));
            if b.converter == ASSIGN {
                return source;
            }
            format!("{} ({source})", in_words(&ft.logic(i).0))
        })
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A place as the loop and replace checks see it: what owns the value (a
/// widget's built-in, a template property, any other slot) and which of
/// its axes, as a mask: the whole vector is every axis, x and y one each
/// (PropertyField 1 whole, 2..5 x..w). Two places share values when they
/// have one owner and their masks meet: writing Size changes Size.x, and
/// Size.x and Size.y do not touch (round 6 took them for a loop).
pub(super) fn place_of(slots: &[Slot], s: usize) -> (String, u8) {
    // A path's last step names the value and its field the axes; the steps
    // before it are where it sits. Round 7 only did this for one-step
    // paths: Layout.TextPlane and Layout.TextPlane.x were strangers.
    let along = |mut owner: String, path: &[(u32, u32)]| {
        use std::fmt::Write as _;
        let Some((&(field, id), up)) = path.split_last() else {
            return (owner, u8::MAX);
        };
        for (f, i) in up {
            let _ = write!(owner, " {f}:{i}");
        }
        let _ = write!(owner, " {id}");
        (owner, axis_mask(field))
    };
    match slots.get(s) {
        Some(Slot::Widget { widget, path }) if !path.is_empty() => {
            along(format!("{widget:?}"), path)
        }
        Some(Slot::Nested {
            widget,
            inner,
            path,
        }) if !path.is_empty() => along(format!("nested {widget} {inner:?}"), path),
        Some(&Slot::Template(p, field)) => (format!("template {p}"), axis_mask(field)),
        _ => (place_key(slots, s), u8::MAX),
    }
}

/// PropertyField 1 (the whole value) is every axis; 2..5 one each.
fn axis_mask(field: u32) -> u8 {
    match field {
        2..=5 => 1 << (field - 2),
        _ => u8::MAX,
    }
}

/// A place as a number: owner index into `Places` and axis mask.
type Place = (u32, u8);

fn meet(a: Place, b: Place) -> bool {
    a.0 == b.0 && a.1 & b.1 != 0
}

/// Every slot's place (`place_of`) numbered, and the bindings writing and
/// reading each owner, worked out once per question. The drag start used
/// to format and compare owner strings per binding per pin: 23-35 ms on
/// the 381-binding Main Container of fd640932 (round 6).
pub(in crate::ui) struct Places {
    of: Vec<Place>,
    owners: HashMap<String, u32>,
    writes: ByOwner,
    reads: ByOwner,
}

impl Places {
    pub(in crate::ui) fn new(
        slots: &[Slot],
        bindings: &[anno_formats::phx::binding::Binding],
    ) -> Self {
        let mut owners = HashMap::new();
        let of: Vec<Place> = (0..slots.len())
            .map(|s| {
                let (owner, mask) = place_of(slots, s);
                let next = owners.len() as u32;
                (*owners.entry(owner).or_insert(next), mask)
            })
            .collect();
        let place = |s: usize| of.get(s).copied();
        let touch = |side: fn(&anno_formats::phx::binding::Binding) -> &[usize]| {
            let list = bindings.iter().enumerate().flat_map(|(i, b)| {
                side(b)
                    .iter()
                    .filter_map(move |&s| place(s).map(|(o, m)| (o, i as u32, m)))
            });
            ByOwner::new(owners.len(), list)
        };
        let writes = touch(|b| &b.targets);
        let reads = touch(|b| &b.sources);
        Self {
            of,
            owners,
            writes,
            reads,
        }
    }

    /// Slot `s`'s place; a slot the table lacks meets nothing.
    pub(super) fn of(&self, s: usize) -> Place {
        self.of.get(s).copied().unwrap_or((u32::MAX, 0))
    }

    /// The place of widget `widget`'s built-in `(field, id)`; one no slot
    /// names meets nothing.
    fn built_in(&self, widget: usize, (field, id): (u32, u32)) -> Place {
        let owner = self.owners.get(&format!("{:?} {id}", Some(widget)));
        (owner.copied().unwrap_or(u32::MAX), axis_mask(field))
    }

    /// Every place the value of slot `from` comes from, through the
    /// bindings that write it, its own included: a wire from `from` onto
    /// any of them closes a loop.
    pub(super) fn upstream(
        &self,
        bindings: &[anno_formats::phx::binding::Binding],
        from: usize,
    ) -> Reached {
        let mut seen = Reached(vec![0; self.owners.len()]);
        let mut queue = vec![self.of(from)];
        while let Some((owner, mask)) = queue.pop() {
            let new = seen.add(owner, mask);
            for &(b, m) in self.writes.get(owner) {
                if m & new != 0 {
                    queue.extend(bindings[b as usize].sources.iter().map(|&s| self.of(s)));
                }
            }
        }
        seen
    }

    /// The bindings that read what slots `from` hold (or an axis or whole
    /// that shares values with them), directly or through other bindings:
    /// everything a change there flows into. Each axis of each place is
    /// visited once, so a loop already in a file ends the walk.
    pub(in crate::ui) fn readers(
        &self,
        bindings: &[anno_formats::phx::binding::Binding],
        from: impl IntoIterator<Item = usize>,
    ) -> std::collections::BTreeSet<usize> {
        let mut seen = Reached(vec![0; self.owners.len()]);
        let mut out = std::collections::BTreeSet::new();
        let mut queue: Vec<Place> = from.into_iter().map(|s| self.of(s)).collect();
        while let Some((owner, mask)) = queue.pop() {
            let new = seen.add(owner, mask);
            for &(b, m) in self.reads.get(owner) {
                if m & new != 0 && out.insert(b as usize) {
                    queue.extend(bindings[b as usize].targets.iter().map(|&t| self.of(t)));
                }
            }
        }
        out
    }
}

/// Bindings by the owner of a place they touch, with its axes: one list
/// sorted by owner, and where each owner's run starts.
struct ByOwner {
    start: Vec<u32>,
    items: Vec<(u32, u8)>,
}

impl ByOwner {
    fn new(owners: usize, list: impl Iterator<Item = (u32, u32, u8)>) -> Self {
        let mut list: Vec<_> = list.collect();
        list.sort_unstable_by_key(|&(o, b, _)| (o, b));
        let mut start = vec![0u32; owners + 1];
        for &(o, ..) in &list {
            start[o as usize + 1] += 1;
        }
        for o in 0..owners {
            start[o + 1] += start[o];
        }
        let items = list.into_iter().map(|(_, b, m)| (b, m)).collect();
        Self { start, items }
    }

    /// (binding, axes) for `owner`, bindings ascending.
    fn get(&self, owner: u32) -> &[(u32, u8)] {
        let o = owner as usize;
        match (self.start.get(o), self.start.get(o + 1)) {
            (Some(&a), Some(&b)) => &self.items[a as usize..b as usize],
            _ => &[],
        }
    }
}

/// The axes reached of each owner: what a walk over places has seen.
pub(super) struct Reached(Vec<u8>);

impl Reached {
    /// Marks `mask` of `owner` as reached; the part not reached before.
    fn add(&mut self, owner: u32, mask: u8) -> u8 {
        let Some(seen) = self.0.get_mut(owner as usize) else {
            return 0;
        };
        let new = mask & !*seen;
        *seen |= mask;
        new
    }

    pub(super) fn meets(&self, (owner, mask): Place) -> bool {
        self.0.get(owner as usize).is_some_and(|m| m & mask != 0)
    }
}

impl Graph {
    /// What a wire dropped on each input pin would mean, node by node and
    /// pin by pin: (node, pin, target).
    pub(super) fn drop_targets(&self) -> Vec<(usize, usize, Drop)> {
        let mut out = Vec::new();
        for (n, node) in self.nodes.iter().enumerate() {
            let widget = node.kind.widget();
            for (k, p) in node.ins.iter().enumerate() {
                let drop = match (p.slot, p.open, widget, p.arg) {
                    _ if p.make.is_some() => Drop::New(p.make.expect("checked")),
                    (Some(s), _, _, _) => Drop::Slot(s),
                    (None, Some(t), Some(w), _) => Drop::Open {
                        widget: w,
                        target: t,
                    },
                    (None, None, _, Some((binding, k))) => Drop::Arg { binding, k },
                    _ if matches!(node.kind, Kind::Logic { also: Some(_), .. }) => Drop::No(
                        "this if / else's condition is read by two bindings; rewire then or else, or right-click to remove it",
                    ),
                    _ => Drop::No("that pin cannot take a wire yet"),
                };
                out.push((n, k, drop));
            }
        }
        out
    }
}

// Cross-family writes in the shipped UI files, by the kinds the editor
// decides with (`bindings::defined_kind`, constants by their stored
// kind): numbers into text, text into an enum, Float ↔ Vector3,
// Vector2/Vector3 into each other, a Vector3 into a colour, an event into
// an action, and 0x16 into a Vector2 (two if / else passes).
// `the_type_check_takes_what_the_game_writes` holds every shipped copy,
// pass and comparison to this, and nothing more.
pub(super) const SHIPPED: [(u32, u32); 11] = [
    (0x01, 0x04),
    (0x03, 0x04),
    (0x04, 0x01),
    (0x04, 0x11),
    (0x03, 0x06),
    (0x06, 0x03),
    (0x05, 0x06),
    (0x06, 0x05),
    (0x06, 0x12),
    (0x16, 0x05),
    (0x09, 0x0a),
];

/// Whether a value of kind `from` can be copied into one of kind `to`
/// with a plain copy or an if / else: the same family (yes/no, number, enum, text, each
/// vector kind), or a conversion the shipped files make themselves. An
/// enum's value is an entry id, never a number to scale or show: no
/// shipped copy takes one out of its family. A kind the editor has no name
/// for (0x0d, 0x0e, 0x10 in the files) is unknown and not refused, and
/// neither is an Object (a view model, 0x13): the shipped files copy
/// numbers and yes/nos into object inputs (27 copies) and objects on.
pub(in crate::ui) fn fits(from: u32, to: u32) -> bool {
    let named = |k: u32| anno_formats::phx::property_type_name(k).is_some() && k != T_OBJECT;
    if !named(from) || !named(to) {
        return true;
    }
    let family = |k: u32| match k {
        0x00 => 0,
        0x01..=0x03 => 1,
        0x04 => 2,
        0x11 => 3,
        k => 10 + k,
    };
    family(from) == family(to) || SHIPPED.contains(&(from, to))
}

/// Whether values of kinds `a` and `b` are compared (Equal, NotEqual) in
/// the shipped files: either way round, the same family or a pair the
/// files compare (an enum with its entry id, a colour with a Vector3
/// constant, a number with "").
pub(super) fn comparable(a: u32, b: u32) -> bool {
    fits(a, b) || fits(b, a)
}

/// A slot of this file as `add_binding` writes it.
fn new_slot(slots: &[Slot], s: usize) -> Result<NewSlot> {
    Ok(match slots.get(s) {
        Some(Slot::Widget {
            widget: Some(w),
            path,
        }) => NewSlot::Widget {
            widget: *w,
            path: path.clone(),
        },
        Some(Slot::Template(p, field)) => NewSlot::Template(*p, *field),
        // A copy keeps the kind stored with the value.
        Some(Slot::Constant { kind, value }) => NewSlot::Constant {
            kind: match (kind, value) {
                (Some(k), _) => *k,
                (None, DefaultValue::Bool(_)) => T_BOOL,
                (None, DefaultValue::F32(_)) => T_FLOAT,
                (None, DefaultValue::Str(_)) => T_STRING,
                _ => bail!("that kind of fixed value cannot be wired yet"),
            },
            value: value.clone(),
        },
        Some(Slot::Widget { widget: None, .. }) => {
            bail!("that property's path does not resolve in this file")
        }
        // A clip's Start or Completed, a property inside a nested template:
        // through the container the file already has for it (round 15: an
        // event could not be wired to a clip at all, only removed).
        Some(Slot::Clip { .. } | Slot::Nested { .. }) => NewSlot::Existing(s),
        None => bail!("no such slot"),
    })
}

impl UiEditor {
    /// Asks before removing `bindings`: a menu at `at` naming what they set.
    pub(super) fn ask_unbind(&mut self, at: Point<Pixels>, bindings: Vec<usize>) {
        let Some(ft) = self.file_table(self.subject()) else {
            return;
        };
        let (slots, all) = (&ft.table.slots, &ft.table.bindings);
        let mut sets: Vec<String> = bindings
            .iter()
            .filter_map(|&b| all.get(b))
            .flat_map(|b| b.targets.iter().map(|&t| ft.label(t)))
            .collect();
        sets.dedup();
        // What reads those targets, binding to binding: the widget
        // properties at the end of it lose their value too.
        let from: Vec<usize> = bindings
            .iter()
            .filter_map(|&b| all.get(b))
            .flat_map(|b| b.targets.iter().copied())
            .collect();
        let mut reach: Vec<String> = Vec::new();
        for b in Places::new(slots, all).readers(all, from) {
            for &t in &all[b].targets {
                if slot_widget(slots, t).is_some() {
                    let l = ft.label(t);
                    if !sets.contains(&l) && !reach.contains(&l) {
                        reach.push(l);
                    }
                }
            }
        }
        let at = at - self.graph.bounds.origin;
        self.graph.menu = Some(UnbindMenu {
            at: [f32::from(at.x), f32::from(at.y)],
            bindings,
            sets,
            reach,
        });
    }

    /// Wires slot `from` of this file into `to`: a plain copy, one undo
    /// step.
    pub(in crate::ui) fn wire_up(
        &mut self,
        from: impl Into<Source>,
        kind: Option<u32>,
        to: Drop,
    ) -> Result<String> {
        let from = from.into();
        self.ensure_editable()?;
        let verdict = match self.wire_verdicts(from, kind, &[to]).remove(0) {
            Ok(said) => said,
            Err(why) => bail!(why),
        };
        // A rewire says what the drop said it would ("_hub01.x's then reads
        // Icon_Size"; round 10's status dropped the branch).
        if let Drop::Arg { binding, k } = to {
            self.rewire_arg(from, binding, k)?;
            return Ok(format!("Rewired: {}", verdict.trim_end_matches(" instead")));
        }
        let widgets = self.asset.widgets();
        let table = self.asset.bindings(&widgets);
        let slots = &table.slots;
        let target = match to {
            Drop::Slot(t) => new_slot(slots, t)?,
            Drop::Open { widget, target } => NewSlot::Widget {
                widget,
                path: target.path(),
            },
            Drop::New(p) => p.slot(),
            Drop::Arg { .. } | Drop::No(_) => unreachable!("refused or rewired above"),
        };
        // Built from this table: removing a binding below renumbers slots.
        let source = match from {
            Source::Slot(s) => new_slot(slots, s)?,
            Source::New(p) => p.slot(),
        };
        // Names as the graph shows them (container inputs, components),
        // not the file's ids; the verdict found the table already.
        let ft = self
            .edited_table()
            .ok_or_else(|| anyhow::anyhow!("no bindings here"))?;
        let label = |s: usize| plain(ft.label(s));
        let mut said = format!(
            "Wired {} → {}",
            match from {
                Source::Slot(s) => label(s),
                Source::New(p) => p.label(&ft),
            },
            match to {
                Drop::Slot(t) => label(t),
                Drop::Open { widget, target } => open_label(&ft, widget, target),
                Drop::New(p) => p.label(&ft),
                Drop::Arg { .. } | Drop::No(_) => unreachable!("handled above"),
            }
        );
        // A value's copies go, in the same undo step: the ones the verdict
        // named, over the same place (a whole vector's drop takes its axes'
        // copies too; round 6 kept them and stacked). An event keeps its
        // triggers and gets one more.
        let mut next = self.asset.clone();
        if let Drop::Slot(t) = to {
            let places = Places::new(&ft.table.slots, &ft.table.bindings);
            match takeover(&ft, &places, t).map_err(anyhow::Error::msg)? {
                Takeover::Joins(ws) if !ws.is_empty() => {
                    said = format!("{said}, alongside {}", writer_names(&ft, &ws));
                }
                Takeover::Joins(_) => {}
                Takeover::Replaces(ws) => {
                    // Said: a replace looks like a plain wire otherwise.
                    if !ws.is_empty() {
                        said = format!("{said}, replacing {}", writer_names(&ft, &ws));
                    }
                    // One copy of exactly this place takes the new source
                    // where it is (round 16: a replaced copy moved to the end
                    // of the file, and file order may decide which of two
                    // clip actions in one frame runs first).
                    if let [w] = ws[..]
                        && ft.table.bindings[w].targets[..] == [t]
                    {
                        self.rewire_arg(from, w, 0)?;
                        return Ok(said);
                    }
                    // The later first, so earlier indices hold.
                    for &w in ws.iter().rev() {
                        next = next.remove_binding(&widgets, w)?;
                    }
                }
            }
        }
        let widgets = next.widgets();
        let next = next.add_binding(
            &widgets,
            &NewBinding {
                converter: ASSIGN,
                sources: vec![source],
                targets: vec![target],
            },
        )?;
        self.replace_asset(next);
        Ok(said)
    }

    /// What a wire from slot `from` (of kind `kind` when the slot does not
    /// say) would do dropped on each of `drops`: what it does, or why not.
    /// One rule for the pins a drag lights up, the line by the cursor and
    /// `wire_up`, which refuses on the same words.
    pub(super) fn wire_verdicts(
        &mut self,
        from: impl Into<Source>,
        kind: Option<u32>,
        drops: &[Drop],
    ) -> Vec<Result<String, String>> {
        let from = from.into();
        // A place the wire makes is an event nothing in the file touches:
        // no binding feeds it, none reads it.
        let slot = match from {
            Source::Slot(s) => Some(s),
            Source::New(_) => None,
        };
        // The table the graph shows, with the kinds and names its templates
        // and components define (Icon3D.IconType is an enum, which the
        // file alone does not say).
        let Some(ft) = self.edited_table() else {
            let why = "these bindings are read only here; edit the template here first";
            return vec![Err(why.to_owned()); drops.len()];
        };
        let (slots, bindings) = (&ft.table.slots, &ft.table.bindings);
        let label = |s: usize| plain(ft.label(s));
        // The source as the graph shows it: a converter's result by the
        // node's title, its place after it ("when it turns false
        // (TruetoFalseChange_02)"); round 15 named only the place.
        let source = match from {
            Source::New(p) => p.label(&ft),
            Source::Slot(from) => match bindings
                .iter()
                .position(|b| b.targets.contains(&from) && b.converter != ASSIGN)
            {
                Some(i) => format!("{} ({})", ft.logic(i).0, label(from)),
                None => label(from),
            },
        };
        // A place of no known kind that is an event by what it meets (a
        // clip's Start, an object's field an event is copied into or out
        // of, a "when it turns false" result kept somewhere) counts as one,
        // as the graph draws it (rounds 15 and 16 let one onto a yes/no
        // input).
        let kind_of = |s: usize| {
            ft.kind(s)
                .or_else(|| (ft.event_slot(s) == Some(true)).then_some(T_EVENT))
        };
        let from_kind = match from {
            Source::Slot(s) => kind_of(s).or(kind),
            Source::New(_) => Some(T_EVENT),
        };
        // What the write would be made of: a clip's or a nested template's
        // property cannot be named in a new binding yet, and green must
        // not promise it (round 5: "reads clip OnMouseLeave" was refused
        // on release).
        let unwritable = slot.and_then(|s| new_slot(slots, s).err().map(|e| format!("{e:#}")));
        // Who writes each place (by owner and axes, see `place_of`), and
        // everything that feeds `from`: a wire onto any of those closes a
        // loop.
        let places = Places::new(slots, bindings);
        let upstream = match slot {
            Some(s) => places.upstream(bindings, s),
            None => Reached(Vec::new()),
        };
        let feeds_from = |t: usize| upstream.meets(places.of(t));
        // Events and values meet only through the converters made for it;
        // the refusal names them (round 15).
        let event = |k: u32| k == T_EVENT || k == T_ACTION;
        let refuse_kind = |want: Option<u32>, pin: &str| match (from_kind, want) {
            (Some(a), Some(b)) if !fits(a, b) => Err(match (event(a), event(b)) {
                (true, false) => format!(
                    "{pin} holds {}, and {source} is an event: it happens and holds no \
                     value (a yes/no follows one through true on event or false on event)",
                    kind_name(b)
                ),
                (false, true) => format!(
                    "{pin} is an event, and {source} is {}, a value (a value sends one \
                     through when it turns true, when it turns false or when it changes)",
                    kind_name(a)
                ),
                // A yes/no where a colour, number or text goes: the game's
                // files choose between two with an if / else (round 15).
                _ if a == T_BOOL => format!(
                    "{pin} takes {}, and {source} is a yes/no (an if / else turns one into \
                     {}: its then and else hold the two)",
                    kind_name(b),
                    kind_name(b)
                ),
                _ => format!(
                    "{pin} takes {}, and {source} is {}",
                    kind_name(b),
                    kind_name(a)
                ),
            }),
            _ => Ok(()),
        };
        let self_loop =
            |to: &str| format!("{source} would read itself: it already follows what {to}");
        drops
            .iter()
            .map(|&drop| match drop {
                Drop::No(why) => Err(why.to_owned()),
                _ if unwritable.is_some() => Err(unwritable.clone().expect("checked")),
                Drop::Arg { binding, k } => {
                    let b = bindings.get(binding).ok_or("that node is gone")?;
                    let &old = b.sources.get(k).ok_or("that input is gone")?;
                    if Some(old) == slot {
                        return Err("That input already reads it".into());
                    }
                    if b.targets.iter().any(|&t| feeds_from(t)) {
                        return Err(self_loop("this node writes"));
                    }
                    // What the converter's argument takes, by what the
                    // game's files feed it (round 5 wired a number into
                    // EnumToBoolean); an argument that takes anything goes
                    // by the other side of a comparison, or by the value
                    // it replaces.
                    let converter = Converter::of(b.converter);
                    match (converter.and_then(|c| c.takes(k)), from_kind) {
                        (Some(takes), Some(a)) if !takes.contains(&a) => {
                            // In the graph's words and with the bridges, as
                            // a property pin says it (round 15: "LogicalOr
                            // takes Boolean there, and TruetoFalseChange_02
                            // is Event").
                            let (title, names) = ft.logic(binding);
                            let title = in_words(&title);
                            let arg = names.get(k).filter(|n| !n.is_empty());
                            let pin = match arg {
                                Some(arg) => format!("{title}'s {arg}"),
                                None => title.to_owned(),
                            };
                            // The kinds it has a name for.
                            let takes_names = takes
                                .iter()
                                .filter(|&&t| anno_formats::phx::property_type_name(t).is_some())
                                .map(|&t| kind_name(t))
                                .collect::<Vec<_>>()
                                .join(" or ");
                            return Err(match (event(a), takes.iter().any(|&t| event(t))) {
                                (true, false) => format!(
                                    "{pin} holds {takes_names}, and {source} is an event: it \
                                     happens and holds no value (a yes/no follows one through \
                                     true on event or false on event)"
                                ),
                                (false, true) => format!(
                                    "{pin} is an event, and {source} is {}, a value (a value \
                                     sends one through when it turns true, when it turns false \
                                     or when it changes)",
                                    kind_name(a)
                                ),
                                _ => format!(
                                    "{pin} takes {takes_names}, and {source} is {}",
                                    kind_name(a)
                                ),
                            });
                        }
                        (Some(_), _) => {}
                        (None, _)
                            if matches!(
                                converter,
                                Some(Converter::Equal | Converter::NotEqual)
                            ) =>
                        {
                            // Compared, not copied: either way round (an enum
                            // with its entry id, as the game's files do).
                            let other = b.sources.get(1 - k.min(1)).and_then(|&o| kind_of(o));
                            if let (Some(a), Some(o)) = (from_kind, other)
                                && !comparable(a, o)
                            {
                                return Err(format!(
                                    "the other side is {}, and {source} is {}",
                                    kind_name(o),
                                    kind_name(a)
                                ));
                            }
                        }
                        // Passed on as it is: what the target takes, not
                        // what the value replaced happens to be (round 7: a
                        // number went green into ColorTint's then and red
                        // into its else).
                        (None, _) if converter.is_some_and(|c| c.passes(k)) => {
                            let to = b.targets[0];
                            refuse_kind(kind_of(to).or(kind_of(old)), &label(to))?
                        }
                        (None, _) => refuse_kind(kind_of(old), "that input")?,
                    }
                    // Name the argument: adjacent pins sit 17 units apart
                    // and both fall in reach at a far zoom.
                    let (_, names) = ft.logic(binding);
                    // A gate pair's value is its then or its else: the
                    // partner is another writer of its place (from the
                    // numbered index, not a string scan of every binding).
                    let pair = (k == 0)
                        .then(|| {
                            let owner = places.of(*b.targets.first()?).0;
                            places.writes.get(owner).iter().find_map(|&(o, _)| {
                                let o = o as usize;
                                // The same place, not another axis of it.
                                let same = |i: usize| place_key(slots, bindings[i].targets[0]);
                                (o != binding && same(o) == same(binding))
                                    .then(|| gate_pair(bindings, slots, binding, o))
                                    .flatten()
                            })
                        })
                        .flatten();
                    // The pass if's value is the then, the pass unless's the else.
                    let names: Vec<String> = match pair {
                        Some((t, _)) => {
                            let (_, ite) = logic_name("IfTrueFalseAssign", 3);
                            vec![ite[usize::from(t != binding)].clone()]
                        }
                        None => names,
                    };
                    let arg = names.get(k).filter(|n| !n.is_empty());
                    let node = label(b.targets[0]);
                    Ok(match arg {
                        Some(a) => format!("{node}'s {a} reads {source} instead"),
                        None => format!("{node} reads {source} instead"),
                    })
                }
                Drop::Slot(t) => {
                    if Some(t) == slot {
                        return Err("A property cannot follow itself".into());
                    }
                    let pin = label(t);
                    refuse_kind(kind_of(t), &pin)?;
                    new_slot(slots, t).map_err(|e| format!("{e:#}"))?;
                    if feeds_from(t) {
                        return Err(self_loop("it would feed"));
                    }
                    // An axis of a vector set whole cannot get a writer of
                    // its own; a whole one replaces copies of its axes too.
                    let at = places.of(t);
                    if at.1 != u8::MAX
                        && let Some(&(w, _)) =
                            places.writes.get(at.0).iter().find(|(_, m)| *m == u8::MAX)
                    {
                        return Err(format!(
                            "{pin} is part of a vector {} sets whole; remove that binding first",
                            label(bindings[w as usize].sources[0])
                        ));
                    }
                    // A clip's Start or Stop reads as what the source does to
                    // the clip: "OnMouseEnter also starts BtnOverlay_DS ›
                    // clip Leave, with OnMouseLeave …".
                    let acts = match slots.get(t) {
                        Some(Slot::Clip { path, .. } | Slot::Nested { path, .. }) => {
                            path.last().and_then(|&(_, id)| clip_verb(id & 0x7fff_ffff))
                        }
                        _ => None,
                    };
                    let does = |also: &str| match acts {
                        Some(verb) => {
                            let clip = pin.rsplit_once(" · ").map_or(pin.as_str(), |(c, _)| c);
                            format!("{source} {also}{verb} {clip}")
                        }
                        None => format!("{pin} {also}follows {source}"),
                    };
                    Ok(match takeover(&ft, &places, t)? {
                        Takeover::Joins(ws) if !ws.is_empty() => {
                            format!("{}, with {}", does("also "), writer_names(&ft, &ws))
                        }
                        Takeover::Joins(_) => does(""),
                        Takeover::Replaces(ws) if ws.is_empty() => does(""),
                        Takeover::Replaces(ws) => {
                            format!("{}, replacing {}", does(""), writer_names(&ft, &ws))
                        }
                    })
                }
                Drop::Open { widget, target } => {
                    let pin = open_label(&ft, widget, target);
                    refuse_kind(Some(target.kind()), &pin)?;
                    let [(field, id)] = target.path()[..] else {
                        unreachable!("open pins are one built-in")
                    };
                    if upstream.meets(places.built_in(widget, (field, id))) {
                        return Err(self_loop("it would feed"));
                    }
                    Ok(format!("{pin} follows {source}"))
                }
                // TODO(alex): a clip's own Started into its Start loops; the
                // game's files never do it, nothing refuses it yet.
                Drop::New(c) => {
                    let pin = c.label(&ft);
                    refuse_kind(Some(T_EVENT), &pin)?;
                    // "starts clip Nothing", not "starts clip Nothing · Start".
                    let clip = pin.rsplit_once(" · ").map_or(pin.as_str(), |(c, _)| c);
                    Ok(format!("{source} {} {clip}", c.verb()))
                }
            })
            .collect()
    }

    /// Feeds argument `k` of converter binding `binding` from slot `from`
    /// instead, in place, as one undo step: the binding keeps its place,
    /// id, mode and settings (see `Asset::set_binding_source`).
    fn rewire_arg(&mut self, from: Source, binding: usize, k: usize) -> Result<()> {
        use anyhow::Context as _;
        let widgets = self.asset.widgets();
        let table = self.asset.bindings(&widgets);
        let slots = &table.slots;
        anyhow::ensure!(binding < table.bindings.len(), "no such binding");
        let source = match from {
            Source::Slot(s) => new_slot(slots, s).context("no such source")?,
            Source::New(p) => p.slot(),
        };
        let next = self
            .asset
            .set_binding_source(&widgets, binding, k, &source)?;
        self.replace_asset(next);
        Ok(())
    }
}
