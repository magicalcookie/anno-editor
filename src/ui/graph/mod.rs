// Owns the binding graph: a file's bindings as nodes with pins, wired left
// to right, read and edited in the Bindings pane. A widget is one node with
// its bound properties as pins; the template's inputs and each data object
// it reads are nodes of their own; converters are logic nodes. Fixed values
// sit on the pin they feed, and a template property that only carries one
// converter's result is that converter's output pin, so the graph shows
// what feeds what and not the table's plumbing. Pins show the value of the
// last layout. PHX stores no positions: the graph lays itself out.
//
// Here: the graph's nodes and pins, built from a file's table. `layout`
// places them, `wiring` says what a wire or a removal does, `view` is the
// pane.
use super::bindings::{
    FileTable, T_EVENT, Target, logic_name, short, slot_kind, slot_widget, value_kind,
};
use anno_formats::phx::binding::{Converter, Slot, builtin};
use std::collections::HashMap;
use wiring::{NewPlace, Places};

mod layout;
#[cfg(test)]
mod tests;
pub(super) mod view;
pub(super) mod wiring;

/// The clip actions an event drives: Start and Stop (no shipped file
/// binds Pause or Resume).
const CLIP_ACTIONS: [u32; 2] = [builtin::START, builtin::STOP];

/// The clip events a wire starts from: Started and Completed (no shipped
/// file reads Stopped, Paused or Resumed).
const CLIP_EVENTS: [u32; 2] = [builtin::STARTED, builtin::COMPLETED];

/// Bindings one graph draws; a whole big file is past reading anyway.
const MAX_BINDINGS: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// A widget of the file, its bound properties as pins.
    Widget { widget: usize, focus: bool },
    /// The template's own inputs that nothing here writes.
    Inputs,
    /// An object the template reads fields of (`Data`, `TextLayout`).
    Data,
    /// A clip's Start, Stop or Completed.
    Clip,
    /// A template property more than one binding writes, or a copy fills.
    Value,
    /// A binding with logic: its converter.
    /// `also`: the second binding of a gate pair shown as one if / else.
    Logic {
        binding: usize,
        also: Option<usize>,
        ran: bool,
    },
}

impl Kind {
    /// The widget a widget node stands for.
    fn widget(self) -> Option<usize> {
        match self {
            Kind::Widget { widget, .. } => Some(widget),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Pin {
    pub name: String,
    pub value: Option<String>,
    /// The slot it stands for, which the editor can bind (None: a
    /// converter's input or output, or a property not bound yet).
    pub slot: Option<usize>,
    /// A property not bound yet that a wire can be dropped on.
    pub open: Option<Target>,
    /// The plain copy that fills it, which unbinding removes.
    pub fed_by: Option<usize>,
    /// A converter's input: (binding, argument), which a wire rewires.
    pub arg: Option<(usize, usize)>,
    /// A fixed value, not a wire.
    pub literal: bool,
    /// That fixed value's stored span in its file, which a click on it
    /// edits in place (an if / else's then and else, a Multiply's factor).
    pub constant: Option<usize>,
    /// The colour that fixed value is, when it is one: drawn as a swatch
    /// that opens a picker.
    pub rgb: Option<[u8; 3]>,
    /// The PropertyType it holds, from the file or else from its value in
    /// the last layout; what a wire from it is checked with.
    pub kind: Option<u32>,
    /// What it stands for, as held rows remember it: a widget property's
    /// path, the same before and after it is bound (round 8: "+ Opacity"
    /// came back as AlphaTint at the bottom); empty: its name.
    pub id: String,
    /// An event wire ends here: it is something that happens (a press, a
    /// clip's Completed), drawn as one, with no value.
    pub event: bool,
    /// What it is for, from the game's Phoenix sources (a button's
    /// OnPress, a tween's StartAlpha), shown on hover.
    pub about: Option<String>,
    /// A place no binding of the file reads or writes yet: a wire dropped
    /// on this input, or started from this output, makes it.
    pub make: Option<NewPlace>,
}

impl Pin {
    fn new(name: String) -> Pin {
        Pin {
            name,
            value: None,
            slot: None,
            open: None,
            fed_by: None,
            arg: None,
            literal: false,
            constant: None,
            rgb: None,
            kind: None,
            id: String::new(),
            event: false,
            about: None,
            make: None,
        }
    }

    /// See `id`.
    fn id(&self) -> &str {
        if self.id.is_empty() {
            &self.name
        } else {
            &self.id
        }
    }
}

#[derive(Clone, Debug)]
struct Node {
    pub kind: Kind,
    pub title: String,
    pub sub: String,
    pub ins: Vec<Pin>,
    pub outs: Vec<Pin>,
    /// Why a converter did not run, when it did not.
    pub note: Option<String>,
    /// Laid out, design units: x, y, width, height.
    pub rect: [f32; 4],
    /// Who it is across rebuilds (its owner, or its binding), for the
    /// places nodes were dragged to.
    pub key: String,
}

#[derive(Clone, Debug)]
struct Wire {
    /// (node, output pin) to (node, input pin).
    pub from: (usize, usize),
    pub to: (usize, usize),
    /// Carries an event (a clip start, a press), not a value.
    pub event: bool,
    /// Carried a value in the last layout.
    pub live: bool,
}

/// A file's bindings, or the ones that feed one widget, laid out.
#[derive(Clone, Debug, Default)]
struct Graph {
    pub nodes: Vec<Node>,
    pub wires: Vec<Wire>,
    pub size: [f32; 2],
    /// Bindings left out past `MAX_BINDINGS`.
    pub dropped: usize,
}

/// Bindings `x` and `y`, two writers of one place, as an if / else: `pass
/// if` one value and `pass unless` another on one condition, each with one
/// target. (pass if, pass unless), or None.
fn gate_pair(
    bindings: &[anno_formats::phx::binding::Binding],
    slots: &[Slot],
    x: usize,
    y: usize,
) -> Option<(usize, usize)> {
    let conv = |i: usize| Converter::of(bindings[i].converter);
    let (t, f) = match (conv(x), conv(y)) {
        (Some(Converter::IfTrueAssign), Some(Converter::IfFalseAssign)) => (x, y),
        (Some(Converter::IfFalseAssign), Some(Converter::IfTrueAssign)) => (y, x),
        _ => return None,
    };
    let (bt, bf) = (&bindings[t], &bindings[f]);
    (bt.sources.len() == 2
        && bf.sources.len() == 2
        && bt.targets.len() == 1
        && bf.targets.len() == 1
        && place_key(slots, bt.sources[1]) == place_key(slots, bf.sources[1]))
    .then_some((t, f))
}

/// Whether binding `b` is a plain copy: Assign, one way, one source. The
/// graph draws it as a wire; a new binding onto its place replaces it.
fn plain_copy(b: &anno_formats::phx::binding::Binding) -> bool {
    Converter::of(b.converter) == Some(Converter::Assign) && b.sources.len() == 1 && b.mode == 0
}

/// Identity of the place a slot names, so bindings meeting there meet at
/// one pin; each constant is its own.
fn place_key(slots: &[Slot], s: usize) -> String {
    match slots.get(s) {
        Some(Slot::Constant { .. }) | None => format!("const {s}"),
        Some(slot) => format!("{slot:?}"),
    }
}

/// A place as node keys name it: widgets by object id, which survives the
/// edits that renumber widgets (`place_key` holds their index).
fn stable_place(ft: &FileTable, s: usize) -> String {
    let id = |w: usize| {
        ft.widgets
            .get(w)
            .map(|wd| super::hierarchy::object_id(&ft.asset, wd.object))
    };
    match ft.table.slots.get(s) {
        Some(Slot::Widget {
            widget: Some(w),
            path,
        }) => format!("{:?} {path:?}", id(*w)),
        Some(Slot::Nested {
            widget,
            inner,
            path,
        }) => format!("{:?} {inner:?} {path:?}", id(*widget)),
        _ => place_key(&ft.table.slots, s),
    }
}

/// Which node a place is a pin of.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Owner {
    Widget(usize),
    /// A widget's values other bindings read, apart from the ones bound
    /// into it, so a widget whose own size feeds its position is no loop.
    Reads(usize),
    Inputs,
    /// A view model, by the widget whose input it is (0: this file's) and
    /// its property uid: the fields bindings read.
    Data(usize, u32),
    /// Fields of a view model bindings write (GameContext's HideCompleted),
    /// apart from the ones read: one node for both drew its own wire
    /// behind itself.
    DataSet(usize, u32),
    Clip(usize),
    Value(String),
}

/// The node a slot belongs to, the node's title and the pin's name; None
/// for constants, which are pin values.
fn owner_of(ft: &FileTable, s: usize) -> Option<(Owner, String, String)> {
    let label = ft.label(s);
    let (who, what) = label.rsplit_once(" · ").unwrap_or(("", &label));
    let what = what.trim_start_matches('-').to_owned();
    // A view model's field (or the view model handed on whole) is a pin
    // on that view model's node, the game data apart from the widgets'
    // properties.
    if let Some((w, uid)) = ft.view_model(s) {
        let field = what
            .split_once('.')
            .map_or("whole object", |(_, f)| f)
            .to_owned();
        return Some((Owner::Data(w, uid), ft.view_model_name(w, uid), field));
    }
    Some(match ft.table.slots.get(s)? {
        Slot::Constant { .. } => return None,
        Slot::Template(..) => (Owner::Inputs, "Inputs".into(), what),
        Slot::Widget { widget, .. } => {
            let w = widget.unwrap_or(0);
            (Owner::Widget(w), owner_title(ft, w), what)
        }
        // On its widget's node, by the steps inside: "clip Enter · Stop"
        // (round 15: three pins read Stop, Stop, Start).
        Slot::Nested { widget, .. } => (
            Owner::Widget(*widget),
            owner_title(ft, *widget),
            label
                .split_once(" › ")
                .map_or(what, |(_, inside)| inside.to_owned()),
        ),
        // The label names it "clip Leave" already ("clip clip Leave" was
        // the title).
        Slot::Clip { clip, .. } => (Owner::Clip(*clip), who.to_owned(), what),
    })
}

fn owner_title(ft: &FileTable, w: usize) -> String {
    match ft.widgets.get(w).and_then(|w| w.name.clone()) {
        Some(name) => name,
        // As the List names it ("root · ZIndex"; round 16: "widget widget").
        None if w == 0 => "root".into(),
        None => "widget".into(),
    }
}

/// Builds a graph: nodes by owner, pins by place.
struct Builder<'a> {
    ft: &'a FileTable,
    g: Graph,
    nodes: HashMap<Owner, usize>,
    /// (node, place) → pin index, per side.
    ins: HashMap<(usize, String), usize>,
    outs: HashMap<(usize, String), usize>,
    /// Template properties shown as their one writer's output pin.
    folded: HashMap<String, usize>,
    logic: HashMap<usize, usize>,
    /// Places a binding of the file writes.
    written: std::collections::HashSet<String>,
}

impl Builder<'_> {
    fn node(&mut self, owner: Owner, title: String) -> usize {
        if let Some(&n) = self.nodes.get(&owner) {
            return n;
        }
        let kind = match &owner {
            Owner::Widget(w) | Owner::Reads(w) => Kind::Widget {
                widget: *w,
                focus: false,
            },
            Owner::Inputs => Kind::Inputs,
            Owner::Data(..) | Owner::DataSet(..) => Kind::Data,
            Owner::Clip(_) => Kind::Clip,
            Owner::Value(_) => Kind::Value,
        };
        let sub = match (&owner, kind) {
            (Owner::Reads(_), _) => "its values".into(),
            (_, Kind::Widget { .. }) => "widget".into(),
            (_, Kind::Inputs) => "this template's inputs".into(),
            // A container's view model is what this file hands it.
            (Owner::Data(w, _) | Owner::DataSet(w, _), _) if *w != 0 => {
                format!("{}'s view model", owner_title(self.ft, *w))
            }
            (Owner::DataSet(..), _) => "view model, fields it writes".into(),
            (_, Kind::Data) => "view model, fields it reads".into(),
            (_, Kind::Clip) => "timeline".into(),
            _ => "template property".into(),
        };
        self.g.nodes.push(Node {
            kind,
            title,
            sub,
            ins: Vec::new(),
            outs: Vec::new(),
            note: None,
            rect: [0.; 4],
            key: self.owner_key(&owner),
        });
        self.nodes.insert(owner, self.g.nodes.len() - 1);
        self.g.nodes.len() - 1
    }

    /// A node's key: a widget by its object id, which survives the edits
    /// that renumber widgets; the other owners by what they are.
    fn owner_key(&self, owner: &Owner) -> String {
        let id = |w: usize| {
            self.ft
                .widgets
                .get(w)
                .map(|wd| super::hierarchy::object_id(&self.ft.asset, wd.object))
        };
        match owner {
            Owner::Widget(w) => format!("widget {:?}", id(*w)),
            Owner::Reads(w) => format!("reads {:?}", id(*w)),
            Owner::Data(w, uid) => format!("data {:?} {uid:08x}", id(*w)),
            Owner::DataSet(w, uid) => format!("data set {:?} {uid:08x}", id(*w)),
            o => format!("{o:?}"),
        }
    }

    /// Where a template property lives: the Inputs node while nothing
    /// writes it, else a node of its own.
    fn owner(&self, s: usize, key: &str) -> Option<(Owner, String, String)> {
        let (owner, title, name) = owner_of(self.ft, s)?;
        Some(match owner {
            Owner::Inputs if self.written.contains(key) => {
                (Owner::Value(name.clone()), name.clone(), name)
            }
            o => (o, title, name),
        })
    }

    /// The pin a wire leaves `slot` from.
    fn out_pin(&mut self, s: usize) -> Option<(usize, usize)> {
        let key = place_key(&self.ft.table.slots, s);
        if let Some(&b) = self.folded.get(&key) {
            return Some((self.logic[&b], 0));
        }
        let (owner, title, name) = self.owner(s, &key)?;
        let owner = match owner {
            Owner::Widget(w) => Owner::Reads(w),
            o => o,
        };
        let n = self.node(owner, title);
        let kind = slot_kind(&self.ft.asset, &self.ft.table.slots, s);
        let at = *self.outs.entry((n, key)).or_insert_with(|| {
            let mut pin = Pin::new(name);
            pin.slot = Some(s);
            pin.kind = kind;
            self.g.nodes[n].outs.push(pin);
            self.g.nodes[n].outs.len() - 1
        });
        Some((n, at))
    }

    /// The pin a wire into `slot` lands on.
    fn in_pin(&mut self, s: usize) -> Option<(usize, usize)> {
        let key = place_key(&self.ft.table.slots, s);
        let (owner, title, name) = self.owner(s, &key)?;
        let owner = match owner {
            Owner::Data(w, uid) => Owner::DataSet(w, uid),
            o => o,
        };
        let n = self.node(owner, title);
        let at = *self.ins.entry((n, key)).or_insert_with(|| {
            let mut pin = Pin::new(name);
            pin.slot = Some(s);
            self.g.nodes[n].ins.push(pin);
            self.g.nodes[n].ins.len() - 1
        });
        Some((n, at))
    }
}

impl Graph {
    /// The bindings of `ft` that feed `widget` (everything upstream of its
    /// properties, through template properties and sibling widgets) and,
    /// with `feeds`, everything its values reach; all of them without a
    /// widget. `open` adds the
    /// widget's properties nothing binds yet, to drop wires on; in a file
    /// edited here, `clips` adds the Start and Stop of clips nothing starts
    /// or stops yet (the file's, and those of the widget's template).
    pub fn build(
        ft: &FileTable,
        widget: Option<usize>,
        open: &[Target],
        clips: bool,
        feeds: bool,
    ) -> Graph {
        let (slots, bindings) = (&ft.table.slots, &ft.table.bindings);
        let mut writers: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, b) in bindings.iter().enumerate() {
            for &t in &b.targets {
                writers.entry(place_key(slots, t)).or_default().push(i);
            }
        }
        let mut chosen: Vec<usize> = match widget {
            None => (0..bindings.len()).collect(),
            Some(w) => {
                let on_widget = |s: usize| slot_widget(slots, s) == Some(w);
                let mut seen: std::collections::HashSet<String> = Default::default();
                let mut queue: Vec<String> = (0..slots.len())
                    .filter(|&s| on_widget(s))
                    .map(|s| place_key(slots, s))
                    .collect();
                let mut chosen = std::collections::BTreeSet::new();
                while let Some(key) = queue.pop() {
                    if !seen.insert(key.clone()) {
                        continue;
                    }
                    for &b in writers.get(&key).into_iter().flatten() {
                        if chosen.insert(b) {
                            queue.extend(bindings[b].sources.iter().map(|&s| place_key(slots, s)));
                        }
                    }
                }
                // What its own events drive, always: a press or a hover is
                // part of what the widget is, and a wire just made from its
                // + OnPress vanished from this view (round 17).
                chosen.extend((0..bindings.len()).filter(|&i| {
                    bindings[i]
                        .sources
                        .iter()
                        .any(|&s| on_widget(s) && ft.event_slot(s) == Some(true))
                }));
                // What its values reach, on request: the answer to "what
                // changes if I change this", which would crowd "why does
                // it look like this".
                if feeds {
                    let own = (0..slots.len()).filter(|&s| on_widget(s));
                    chosen.extend(Places::new(slots, bindings).readers(bindings, own));
                }
                chosen.into_iter().collect()
            }
        };
        let dropped = chosen.len().saturating_sub(MAX_BINDINGS);
        chosen.truncate(MAX_BINDINGS);
        let copy = |i: usize| plain_copy(&bindings[i]);
        let mut b = Builder {
            ft,
            g: Graph {
                dropped,
                ..Graph::default()
            },
            nodes: HashMap::new(),
            ins: HashMap::new(),
            outs: HashMap::new(),
            folded: HashMap::new(),
            logic: HashMap::new(),
            written: writers.keys().cloned().collect(),
        };
        // A template property one converter alone writes is its output.
        for (key, ws) in &writers {
            if let [w] = ws[..]
                && !copy(w)
                && chosen.contains(&w)
                && bindings[w].targets.len() == 1
                && matches!(slots.get(bindings[w].targets[0]), Some(Slot::Template(..)))
            {
                b.folded.insert(key.clone(), w);
            }
        }
        // Two gates on one place and one condition, `pass if` one value and
        // `pass unless` another, are an if / else: one node, the place its
        // output (`_hub01.x` = 0 if the icon is empty, else its size).
        // `pair` maps the pass-if binding to its partner, `follows` back.
        let mut pair: HashMap<usize, usize> = HashMap::new();
        let mut follows: HashMap<usize, usize> = HashMap::new();
        for (key, ws) in &writers {
            let [x, y] = ws[..] else { continue };
            if !chosen.contains(&x) || !chosen.contains(&y) {
                continue;
            }
            let Some((t, f)) = gate_pair(bindings, slots, x, y) else {
                continue;
            };
            let bt = &bindings[t];
            pair.insert(t, f);
            follows.insert(f, t);
            if matches!(slots.get(bt.targets[0]), Some(Slot::Template(..))) {
                b.folded.insert(key.clone(), t);
            }
        }
        // Each binding's source k lands on this input of its node.
        // IfTrueFalseAssign stores (then, else, if); every if / else node
        // lists them if, then, else, as the gate pair's does.
        let ite = |i: usize| {
            Converter::of(bindings[i].converter) == Some(Converter::IfTrueFalseAssign)
                && bindings[i].sources.len() == 3
        };
        let port = |i: usize, k: usize| match (pair.contains_key(&i), follows.contains_key(&i)) {
            (true, _) => [1, 0][k],
            (_, true) => [2, 0][k],
            _ if ite(i) => [1, 2, 0][k],
            _ => k,
        };
        // Logic nodes first, so folded outputs have a node to be.
        for &i in &chosen {
            if copy(i) || follows.contains_key(&i) {
                continue;
            }
            let bd = &bindings[i];
            let also = pair.get(&i).copied();
            let (title, ports) = match also {
                // An if / else's names, listed if, then, else.
                Some(_) => {
                    let (title, mut ports) = logic_name("IfTrueFalseAssign", 3);
                    ports.rotate_right(1);
                    (title, ports)
                }
                None => ft.logic(i),
            };
            let (ran, outcome) = ft.outcome(i);
            let ran = ran || also.is_some_and(|f| ft.outcome(f).0);
            let out_name = bd
                .targets
                .first()
                .filter(|&&t| b.folded.contains_key(&place_key(slots, t)))
                .and_then(|&t| owner_of(ft, t).map(|o| o.2))
                .unwrap_or_else(|| "out".into());
            b.g.nodes.push(Node {
                kind: Kind::Logic {
                    binding: i,
                    also,
                    ran,
                },
                title: if bd.mode == 1 {
                    format!("{title} ⇄")
                } else {
                    title
                },
                // Two bindings under one if / else face: the same title
                // as a one-binding if / else, with other edit rules (its if
                // refuses a wire, Delete removes both; round 10).
                sub: if also.is_some() {
                    "2 bindings".into()
                } else {
                    String::new()
                },
                // Its inputs take wires (one argument rewired). A gate
                // pair's then and else are each one binding's value (round
                // 8 refused them, and "remove, then wire anew" lost the
                // if / else); its if is both bindings' condition.
                // In display order: source k sits at pin `port(i, k)`.
                ins: {
                    let mut pins: Vec<Pin> =
                        ports.iter().map(|_| Pin::new(String::new())).collect();
                    for (k, name) in ports.into_iter().enumerate() {
                        let at = if also.is_some() { k } else { port(i, k) };
                        pins[at].name = name;
                        pins[at].arg = match also {
                            None => Some((i, k)),
                            Some(f) => [None, Some((i, 0)), Some((f, 0))][k],
                        };
                    }
                    pins
                },
                // Its output reads as what it writes, so a wire can start
                // there: a copy of that property.
                outs: vec![{
                    let mut pin = Pin::new(out_name);
                    pin.slot = bd.targets.first().copied();
                    pin.kind = pin.slot.and_then(|t| slot_kind(&ft.asset, slots, t));
                    pin
                }],
                note: (!ran).then_some(outcome),
                rect: [0.; 4],
                // By what it writes, not its binding index, which moves
                // when a binding before it goes. Two writers of one place
                // are told apart by order below.
                key: format!(
                    "logic {} {}",
                    ft.converter(i),
                    bd.targets
                        .first()
                        .map(|&t| stable_place(ft, t))
                        .unwrap_or_default()
                ),
            });
            b.logic.insert(i, b.g.nodes.len() - 1);
            if let Some(f) = also {
                b.logic.insert(f, b.g.nodes.len() - 1);
            }
        }
        for &i in &chosen {
            let bd = &bindings[i];
            let trace = ft.trace(i);
            let arg = |k: usize| {
                trace
                    .as_ref()
                    .filter(|t| t.args.len() == bd.sources.len())
                    .and_then(|t| t.args.get(k))
                    .map(short)
            };
            let result = trace.as_ref().and_then(|t| t.value.as_ref()).map(short);
            let arg_kind = |k: usize| {
                trace
                    .as_ref()
                    .filter(|t| t.args.len() == bd.sources.len())
                    .and_then(|t| t.args.get(k))
                    .and_then(value_kind)
            };
            // Per wire, not per binding: a FalseToTrueChange reads a value
            // and sends an event, an EventToTrue the other way round
            // (round 15: one flag per binding drew most events as data).
            let flow = ft.flow(i);
            let constant = |s: usize| match slots.get(s) {
                Some(Slot::Constant { value, .. }) => Some(short(value)),
                _ => None,
            };
            // The PropertyType a fixed value is stored as.
            let stored = |s: usize| match slots.get(s) {
                Some(Slot::Constant { kind, .. }) => *kind,
                _ => None,
            };
            // A fixed value's span where it types in place, and the colour
            // it is when it is one.
            let fixed = |s: usize| {
                let span = ft
                    .table
                    .constants
                    .get(&s)
                    .copied()
                    .filter(|&sp| super::typed_text(&ft.asset, sp).is_some());
                let rgb = span.and_then(|sp| super::constant_rgb(&ft.asset, sp, stored(s)));
                (span, rgb)
            };
            // Where the result goes: pins, unless folded into the node (a
            // gate pair's second binding goes where its first does).
            let mut targets = Vec::new();
            for &t in &bd.targets {
                let key = place_key(slots, t);
                if follows.contains_key(&i)
                    || b.folded.get(&key) == Some(&i)
                    || follows
                        .get(&i)
                        .is_some_and(|l| b.folded.get(&key) == Some(l))
                {
                    continue;
                }
                if let Some(p) = b.in_pin(t) {
                    targets.push(p);
                }
            }
            if copy(i) {
                let s = bd.sources[0];
                let value = result.clone().or_else(|| arg(0));
                if let Some(v) = constant(s) {
                    let (span, rgb) = fixed(s);
                    for &(n, p) in &targets {
                        let pin = &mut b.g.nodes[n].ins[p];
                        pin.value = Some(v.clone());
                        pin.literal = true;
                        pin.constant = span;
                        pin.rgb = rgb;
                        pin.fed_by = Some(i);
                    }
                    continue;
                }
                let Some(from) = b.out_pin(s) else { continue };
                let out = &mut b.g.nodes[from.0].outs[from.1];
                if out.value.is_none() {
                    out.value = value.clone();
                }
                if out.kind.is_none() {
                    out.kind = arg_kind(0);
                }
                for &(n, p) in &targets {
                    let pin = &mut b.g.nodes[n].ins[p];
                    if value.is_some() {
                        pin.value = value.clone();
                    }
                    pin.fed_by = Some(i);
                    b.g.wires.push(Wire {
                        from,
                        to: (n, p),
                        event: flow.out,
                        live: value.is_some(),
                    });
                }
                continue;
            }
            let node = b.logic[&i];
            for (k, &s) in bd.sources.iter().enumerate() {
                let to = port(i, k);
                if let Some(v) = constant(s) {
                    let pin = &mut b.g.nodes[node].ins[to];
                    pin.value = Some(v);
                    pin.literal = true;
                    (pin.constant, pin.rgb) = fixed(s);
                    continue;
                }
                if arg(k).is_some() {
                    b.g.nodes[node].ins[to].value = arg(k);
                }
                let Some(from) = b.out_pin(s) else { continue };
                let out = &mut b.g.nodes[from.0].outs[from.1];
                if out.value.is_none() {
                    out.value = arg(k);
                }
                if out.kind.is_none() {
                    out.kind = arg_kind(k);
                }
                // A gate pair reads its condition once.
                if b.g
                    .wires
                    .iter()
                    .any(|w| w.from == from && w.to == (node, to))
                {
                    continue;
                }
                b.g.wires.push(Wire {
                    from,
                    to: (node, to),
                    event: flow.args.get(k).copied().unwrap_or(false),
                    live: arg(k).is_some(),
                });
            }
            if result.is_some() || !follows.contains_key(&i) {
                b.g.nodes[node].outs[0].value = result.clone();
            }
            for &(n, p) in &targets {
                if result.is_some() {
                    b.g.nodes[n].ins[p].value = result.clone();
                }
                b.g.wires.push(Wire {
                    from: (node, 0),
                    to: (n, p),
                    event: flow.out,
                    live: result.is_some(),
                });
            }
        }
        // Whether a binding of the file writes a place: one this view leaves
        // out is bound all the same, and gets no + pin (a drop there would
        // add a second writer).
        let written = |at: &dyn Fn(&Slot) -> bool| {
            bindings
                .iter()
                .any(|b| b.targets.iter().any(|&t| slots.get(t).is_some_and(at)))
        };
        let read = |at: &dyn Fn(&Slot) -> bool| {
            bindings
                .iter()
                .any(|b| b.sources.iter().any(|&t| slots.get(t).is_some_and(at)))
        };
        // A container's template clips nothing starts or stops yet, as +
        // pins on its node: an event can start one (a button's overlay
        // Enter).
        let clip_pins = |w: usize, node: &mut Node| {
            let Some(t) = ft.template(w) else { return };
            for (c, id) in t.clip_ids().into_iter().enumerate() {
                let Some(id) = id else { continue };
                let name = &t.clips()[c].name;
                for action in CLIP_ACTIONS {
                    let what = builtin::name(action).expect("named");
                    let path = [(1, 0x8000_0000 | action)];
                    let bound = written(&|s: &Slot| {
                        matches!(s, Slot::Nested { widget: x, inner, path: q }
                            if *x == w && inner[..] == [id] && q[..] == path)
                    });
                    if !bound {
                        let mut pin = Pin::new(format!("+ clip {name} · {what}"));
                        pin.id = format!("clip {name} · {what}");
                        pin.event = true;
                        pin.make = Some(NewPlace::NestedClip {
                            widget: w,
                            clip: id,
                            id: action,
                        });
                        node.ins.push(pin);
                    }
                }
            }
        };
        // The widget asked about: marked, with what it could still bind;
        // none when nothing binds it and nothing could (the template's
        // root, whose data paths are Data nodes), not an empty box. In a
        // file edited here it is being authored: its + pins show.
        let author = widget.filter(|_| clips && !open.is_empty());
        if let Some(w) =
            widget.filter(|&w| !open.is_empty() || b.nodes.contains_key(&Owner::Widget(w)))
        {
            let n = b.node(Owner::Widget(w), owner_title(ft, w));
            let node = &mut b.g.nodes[n];
            node.kind = Kind::Widget {
                widget: w,
                focus: true,
            };
            // Bound already: the same path, or the whole vector an axis is
            // part of (Position covers X). Labels differ (Opacity is
            // AlphaTint), paths do not.
            let paths: Vec<&Vec<(u32, u32)>> = node
                .ins
                .iter()
                .filter_map(|p| match slots.get(p.slot?) {
                    Some(Slot::Widget { path, .. }) => Some(path),
                    _ => None,
                })
                .collect();
            let bound = |t: Target| {
                let want = t.path();
                paths.iter().any(|path| {
                    **path == want
                        || matches!((&path[..], &want[..]), ([(1, id)], [(_, w)]) if id == w)
                })
            };
            let missing: Vec<Target> = open.iter().copied().filter(|&t| !bound(t)).collect();
            for t in missing {
                // Named as it will be once bound (AlphaTint, Position.x).
                let mut pin = Pin::new(format!("+ {}", t.label()));
                pin.open = Some(t);
                node.ins.push(pin);
            }
            if clips && !open.is_empty() {
                clip_pins(w, node);
            }
            // The events its widget type declares that nothing reads yet
            // (a new button's OnPress), on its values node: wires start
            // there.
            if clips && !open.is_empty() {
                let events: Vec<(u32, String)> = ft
                    .type_events(w)
                    .into_iter()
                    .filter(|(uid, _)| {
                        !read(&|s: &Slot| {
                            matches!(s, Slot::Widget { widget: Some(x), path }
                                if *x == w && matches!(path[..], [(_, u)] if u == *uid))
                        })
                    })
                    .collect();
                if !events.is_empty() {
                    let n = b.node(Owner::Reads(w), owner_title(ft, w));
                    for (uid, name) in events {
                        let mut pin = Pin::new(format!("+ {name}"));
                        pin.id = name;
                        pin.event = true;
                        pin.kind = Some(T_EVENT);
                        pin.make = Some(NewPlace::Event { widget: w, uid });
                        b.g.nodes[n].outs.push(pin);
                    }
                }
            }
        }
        // Every other container a widget's view shows offers its template's
        // clips too: a press starts a sibling overlay's clip (round 17: only
        // the selected container had them).
        if clips && widget.is_some() {
            let mut others: Vec<(usize, usize)> = b
                .nodes
                .iter()
                .filter_map(|(o, &n)| match *o {
                    Owner::Widget(w) if Some(w) != widget => Some((n, w)),
                    _ => None,
                })
                .collect();
            others.sort_unstable();
            for (n, w) in others {
                clip_pins(w, &mut b.g.nodes[n]);
            }
        }
        // The file's own clips: on the nodes the graph has; with the whole
        // file every clip, those nothing binds too (round 16: clip Nothing
        // had no node, the hover clips no Start); and every clip while the
        // authored widget sends events, so a new button's press reaches a
        // clip nothing starts yet in one view (round 17).
        let sends = author.is_some_and(|w| !ft.type_events(w).is_empty());
        if clips {
            for (c, id) in ft.asset.clip_ids().into_iter().enumerate() {
                if id.is_none()
                    || (widget.is_some() && !sends && !b.nodes.contains_key(&Owner::Clip(c)))
                {
                    continue;
                }
                let n = b.node(Owner::Clip(c), format!("clip {}", ft.asset.clips()[c].name));
                for action in CLIP_ACTIONS {
                    let what = builtin::name(action).expect("named");
                    let path = [(1, 0x8000_0000 | action)];
                    let node = &mut b.g.nodes[n];
                    let bound = written(
                        &|s: &Slot| matches!(s, Slot::Clip { clip, path: q } if *clip == c && q[..] == path),
                    );
                    if !bound {
                        let mut pin = Pin::new(format!("+ {what}"));
                        pin.id = what.to_owned();
                        pin.event = true;
                        pin.make = Some(NewPlace::Clip {
                            clip: c,
                            id: action,
                        });
                        node.ins.push(pin);
                    }
                }
                // What it sends that nothing reads yet: its Started and
                // Completed, wires to start from.
                for id in CLIP_EVENTS {
                    let what = builtin::name(id).expect("named");
                    let path = [(1, 0x8000_0000 | id)];
                    let node = &mut b.g.nodes[n];
                    let used = read(
                        &|s: &Slot| matches!(s, Slot::Clip { clip, path: q } if *clip == c && q[..] == path),
                    );
                    if !used {
                        let mut pin = Pin::new(format!("+ {what}"));
                        pin.id = what.to_owned();
                        pin.event = true;
                        pin.kind = Some(T_EVENT);
                        pin.make = Some(NewPlace::Clip { clip: c, id });
                        node.outs.push(pin);
                    }
                }
            }
            // The screen showing, nothing reads yet: the source that
            // starts a clip when the file opens, as 99 shipped files copy
            // their root's Shown into a clip's Start (round 17: "start it
            // when the screen shows" had no pin).
            let shown = 0x8000_0000 | builtin::SHOWN;
            let used = read(&|s: &Slot| {
                matches!(s, Slot::Widget { widget: Some(0) | None, path }
                    if matches!(path[..], [(_, id)] if id == shown))
            });
            if widget.is_none() && !used && !ft.asset.clips().is_empty() {
                let n = b.node(Owner::Reads(0), owner_title(ft, 0));
                let what = builtin::name(builtin::SHOWN).expect("named");
                let mut pin = Pin::new(format!("+ {what}"));
                pin.id = what.to_owned();
                pin.event = true;
                pin.kind = Some(T_EVENT);
                pin.make = Some(NewPlace::Event {
                    widget: 0,
                    uid: shown,
                });
                b.g.nodes[n].outs.push(pin);
            }
        }
        let mut g = b.g;
        // An enum's value by its entry's name where the pin's enum is known
        // (a template input, a component property, a label's alignment),
        // and on what reads it: AlignmentX 1 reads Center. Hashed ids are
        // named already (`short`).
        let name_in = |enum_asset: Option<anno_formats::phx::Guid>, pin: &mut Pin| {
            let (Some(g), Some(v)) = (enum_asset, &pin.value) else {
                return;
            };
            if let Some(n) = v
                .parse::<u32>()
                .ok()
                .and_then(|id| anno_formats::phx::enum_entry_name(g, id))
            {
                pin.value = Some(n);
            }
        };
        let mut enum_out = HashMap::new();
        for (n, node) in g.nodes.iter_mut().enumerate() {
            for (k, p) in node.outs.iter_mut().enumerate() {
                let e = p.slot.and_then(|s| ft.enum_of(s));
                if e.is_some() {
                    enum_out.insert((n, k), e);
                }
                name_in(e, p);
                p.about = p.slot.and_then(|s| ft.description(s));
            }
            for p in &mut node.ins {
                name_in(p.slot.and_then(|s| ft.enum_of(s)), p);
                p.about = p.slot.and_then(|s| ft.description(s));
            }
        }
        for w in &g.wires {
            if let Some(&e) = enum_out.get(&w.from) {
                name_in(e, &mut g.nodes[w.to.0].ins[w.to.1]);
            }
        }
        // A fixed value beside an enum reads as its entry too: an Equal's
        // other side, or the place an if / else passes it to (AlignmentX's
        // 1 is Center).
        for n in 0..g.nodes.len() {
            if !matches!(g.nodes[n].kind, Kind::Logic { .. }) {
                continue;
            }
            let near = g.wires.iter().find_map(|w| {
                if w.to.0 == n {
                    enum_out.get(&w.from).copied().flatten()
                } else if w.from.0 == n {
                    g.nodes[w.to.0].ins[w.to.1].slot.and_then(|s| ft.enum_of(s))
                } else {
                    None
                }
            });
            if near.is_some() {
                for p in g.nodes[n].ins.iter_mut().filter(|p| p.literal) {
                    name_in(near, p);
                }
            }
        }
        // A pin an event wire ends at is an event, whatever its last value
        // (an EventToTrue's input, a FalseToTrueChange's output).
        for k in 0..g.wires.len() {
            let w = &g.wires[k];
            if w.event {
                let (from, to) = (w.from, w.to);
                g.nodes[from.0].outs[from.1].event = true;
                g.nodes[to.0].ins[to.1].event = true;
            }
        }
        for p in g
            .nodes
            .iter_mut()
            .flat_map(|n| n.ins.iter_mut().chain(&mut n.outs))
        {
            let path = match (p.open, p.slot.and_then(|s| slots.get(s))) {
                (Some(t), _) => Some(t.path()),
                (None, Some(Slot::Widget { path, .. })) => Some(path.clone()),
                _ => None,
            };
            if let Some(path) = path {
                p.id = format!("{path:?}");
            }
        }
        if widget.is_none() {
            g.split_hubs();
        }
        // Keys repeat when two logic nodes write one place: number them.
        let mut seen: HashMap<String, usize> = HashMap::new();
        for n in &mut g.nodes {
            let k = seen.entry(n.key.clone()).or_default();
            if *k > 0 {
                n.key = format!("{} #{k}", n.key);
            }
            *k += 1;
        }
        g.layout();
        g
    }
}
