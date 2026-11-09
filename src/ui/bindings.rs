// Owns data bindings in the editor: the Bindings pane (the bindings of the
// selected widget's file that read or write it, or all of them, with what
// each read and wrote in the last layout), the inspector's Data section
// that binds a widget property to game data, an input or a value, and the
// catalog of game data the install's templates bind. Also where the
// editor's words for bindings come from: slots (`FileTable::label`, and
// `slot_label` for the timeline's lanes), converters (`logic_name`),
// values (`short`) and kinds; built-in property names are
// `binding::builtin::name`.
use super::UiEditor;
use crate::editor::{Editor, empty_pane};
use anno_formats::phx::{
    Asset, DefaultValue, Guid, Widget,
    binding::{Converter, Slot, builtin},
    name_crc,
};
use gpui_editor_kit::{
    field::{Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{Tooltip, button, info_row, panel_header, section, vscroll},
};

/// A slot label for messages: without the `-` template property names
/// carry in the file, as the graph's pins show them.
pub(super) fn plain(label: String) -> String {
    label
        .replace(" · -", " · ")
        .trim_start_matches('-')
        .to_owned()
}

/// Names a widget's property path that `asset` alone cannot: see
/// [`slot_label`].
pub(super) type PathName<'a> = dyn Fn(usize, &[(u32, u32)]) -> Option<String> + 'a;

/// What a slot is, in words: "Glow · AlphaTint", "-Size", "15",
/// "BtnOverlay_DS › clip Enter · Stop". `input(widget, path)` names what
/// `asset` alone cannot: a container's template inputs (`[uid]`,
/// properties of the container's template) and component properties
/// (`[component uid, property uid]`, properties of the ComponentTemplate).
/// `steps` names what a nested slot walks through below its container
/// ([`nested_steps`]). The Bindings pane names through
/// [`FileTable::label`], the timeline its lanes with what its templates
/// tell.
pub(super) fn slot_label(
    asset: &Asset,
    widgets: &[Widget],
    slots: &[Slot],
    slot: usize,
    input: &PathName<'_>,
    steps: &[String],
) -> String {
    let name = |w: usize| {
        widgets
            .get(w)
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| "?".into())
    };
    match slots.get(slot) {
        Some(Slot::Widget { widget, path }) => {
            let who = match widget {
                Some(0) | None => "root".to_owned(),
                Some(w) => name(*w),
            };
            let what = match (widget, &path[..]) {
                (Some(w), [(field, uid)]) if uid & 0x8000_0000 == 0 => {
                    input(*w, path).map(|n| format!("{n}{}", axis(*field)))
                }
                (Some(w), [(_, c), (field, _)]) if c & 0x8000_0000 == 0 => {
                    input(*w, path).map(|n| format!("{n}{}", axis(*field)))
                }
                _ => None,
            };
            format!(
                "{who} · {}",
                what.unwrap_or_else(|| path_label(asset, path))
            )
        }
        // Steps not known (a template not in the install) read as `…`:
        // "BtnOverlay_DS › · Start" said nothing (round 15, R16O5).
        Some(Slot::Nested { widget, path, .. }) => format!(
            "{} › {} · {}",
            name(*widget),
            if steps.is_empty() {
                "…".to_owned()
            } else {
                steps.join(" › ")
            },
            path_label(asset, path)
        ),
        Some(Slot::Clip { clip, path }) => {
            let clip = asset
                .clips()
                .get(*clip)
                .map_or_else(|| "clip".to_owned(), |c| format!("clip {}", c.name));
            format!("{clip} · {}", path_label(asset, path))
        }
        Some(Slot::Template(i, field)) => {
            let p = asset.properties().get(*i).map_or_else(
                || "template property".to_owned(),
                |p| asset.property_name(p).to_owned(),
            );
            format!("{p}{}", axis(*field))
        }
        Some(Slot::Constant { value, .. }) => short(value),
        None => "?".into(),
    }
}

/// The names of what a nested slot walks through below its container:
/// the widgets `inner` names by object id, from the container's
/// `template` down, the last maybe one of its clips (round 15:
/// "BtnOverlay_DS › · Start" named no clip). `open` reads the template a
/// step's widget shows. Stops at the first step it cannot name.
pub(super) fn nested_steps(
    template: std::sync::Arc<Asset>,
    inner: &[Guid],
    open: &mut dyn FnMut(Guid) -> Option<std::sync::Arc<Asset>>,
) -> Vec<String> {
    let mut at = template;
    let mut names = Vec::new();
    for (k, id) in inner.iter().enumerate() {
        let ws = at.widgets();
        if let Some(&i) = at.widget_ids(&ws).get(id) {
            names.push(ws[i].name.clone().unwrap_or_else(|| "widget".into()));
            let deeper = at.container_template(&ws[i]).and_then(|r| open(r.id));
            match deeper {
                Some(t) if k + 1 < inner.len() => at = t,
                _ if k + 1 < inner.len() => break,
                _ => {}
            }
        } else if let Some(c) = at.clip_ids().iter().position(|c| c.as_ref() == Some(id)) {
            names.push(format!("clip {}", at.clips()[c].name));
        } else {
            break;
        }
    }
    names
}

/// The suffix for PropertyField `field`: 2–5 pick x, y, z, w.
fn axis(field: u32) -> &'static str {
    match field {
        2 => ".x",
        3 => ".y",
        4 => ".z",
        5 => ".w",
        _ => "",
    }
}

/// A widget property path: builtins by name, data paths by their ids.
pub(super) fn path_label(asset: &Asset, path: &[(u32, u32)]) -> String {
    match path {
        [(field, id)] if id & 0x8000_0000 != 0 => {
            let id = id & 0x7fff_ffff;
            let what = builtin::name(id).map_or_else(|| format!("builtin {id:#x}"), str::to_owned);
            format!("{what}{}", axis(*field))
        }
        // A template input of a container, or the root's WidgetType property.
        [(field, uid)] => {
            let name = asset
                .properties()
                .iter()
                .find(|p| p.uid == *uid)
                .map_or_else(
                    || format!("{uid:08x}"),
                    |p| asset.property_name(p).to_owned(),
                );
            format!("{name}{}", axis(*field))
        }
        // A data path: a template property (or WidgetType property) of
        // object type, then game-object fields.
        _ => {
            let name = |id: u32| {
                asset
                    .properties()
                    .iter()
                    .find(|p| p.uid == id)
                    .map(|p| asset.property_name(p).to_owned())
                    .or_else(|| anno_formats::phx::data_field_name(id).map(str::to_owned))
            };
            // A component whose instance sits in a template that does not
            // ship (the construction bar's, kept from Anno 1800): the
            // property by the component JSON, the instance by its id.
            if let [(_, c), (field, uid)] = path[..]
                && name(c).is_none()
                && let Some((component, property)) = anno_formats::phx::component_property_name(uid)
            {
                return format!("{component} {c:08x}.{property}{}", axis(field));
            }
            path.iter()
                .map(|&(_, id)| name(id).unwrap_or_else(|| format!("{id:08x}")))
                .collect::<Vec<_>>()
                .join(".")
        }
    }
}

/// A value in a few characters: floats trimmed, vectors in brackets.
pub(super) fn short(v: &DefaultValue) -> String {
    let f = |bits: u32| {
        let x = f32::from_bits(bits);
        let s = format!("{x:.3}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        if s == "-0" {
            "0".to_owned()
        } else {
            s.to_owned()
        }
    };
    match v {
        DefaultValue::None => "—".into(),
        DefaultValue::Bool(b) => (*b != 0).to_string(),
        DefaultValue::S32(v) => v.to_string(),
        // A hashed enum entry by its name (Orientation's 1505389363 is
        // Horizontal); other numbers as they are.
        DefaultValue::U32(v) => {
            anno_formats::phx::unique_entry_name(*v).unwrap_or_else(|| v.to_string())
        }
        DefaultValue::F32(bits) => f(*bits),
        DefaultValue::Vec(v) => format!(
            "[{}]",
            v.iter().map(|b| f(*b)).collect::<Vec<_>>().join(", ")
        ),
        DefaultValue::Str(s) => format!("\"{s}\""),
        DefaultValue::ObjectId(g) => g.to_string(),
        DefaultValue::Object(class) => format!("object {class:08x}"),
    }
}

/// The Assign converter: a plain copy.
pub(super) const ASSIGN: u32 = name_crc(b"AssignConverterInstance");

/// A converter as the graph, the List and messages name it, and its
/// inputs: the order the shipped files tell (`binding::Converter`), a, b, c
/// otherwise.
pub(super) fn logic_name(name: &str, inputs: usize) -> (String, Vec<String>) {
    let (title, named): (&str, &[&str]) = match name {
        "Assign" => ("copy", &[""]),
        "TwoWayAssign" => ("two-way copy", &[""]),
        "Add" => ("+  Add", &[]),
        "Subtract" => ("−  Subtract", &[]),
        "Multiply" => ("×  Multiply", &[]),
        "Divide" => ("÷  Divide", &[]),
        "Max" => ("max", &[]),
        "Min" => ("min", &[]),
        "Equal" => ("=  Equal", &[]),
        "NotEqual" => ("≠  Not equal", &[]),
        "Greater" => (">  Greater", &[]),
        "GreaterOrEqual" => ("≥  At least", &[]),
        "LogicalAnd" => ("and", &[]),
        "LogicalOr" => ("or", &[]),
        "LogicalInvert" => ("not", &[""]),
        "IfTrueAssign" => ("pass if", &["value", "if"]),
        "IfFalseAssign" => ("pass unless", &["value", "unless"]),
        "IfTrueFalseAssign" => ("if / else", &["then", "else", "if"]),
        "Clamp" => ("clamp", &["value", "min", "max"]),
        // One input each: named after what it takes, not left a bare dot
        // (round 13: FalseToTrueChange's input had no name).
        // Where values become events and back, in words: the raw names
        // read as more arithmetic.
        "FalseToTrueChange" => ("when it turns true", &["value"]),
        "TrueToFalseChange" => ("when it turns false", &["value"]),
        "ValueChange" => ("when it changes", &["value"]),
        "EventToTrue" => ("true on event", &["event"]),
        "EventToFalse" => ("false on event", &["event"]),
        "EventAssign" if inputs == 2 => ("set on event", &["value", "event"]),
        "IsEmpty" | "Floor" | "Round" | "Abs" | "Sign" | "SquareRoot" => (name, &["value"]),
        "EnumToBoolean" => (name, &["enum"]),
        "GetCount" => (name, &["list"]),
        "Sin" | "Cos" => (name, &["angle"]),
        "DegreeToRadian" => (name, &["degrees"]),
        other => (other, &[]),
    };
    let names = (0..inputs)
        .map(|k| match named.get(k) {
            Some(s) => (*s).to_owned(),
            None if inputs == 1 => "in".to_owned(),
            None => ((b'a' + k.min(25) as u8) as char).to_string(),
        })
        .collect();
    (title.to_owned(), names)
}

/// A converter's title in a sentence: "×  Multiply" reads "Multiply".
pub(super) fn in_words(title: &str) -> &str {
    title.rsplit("  ").next().unwrap_or(title)
}

/// One slot of a binding: its label, the widget it names in the file (a
/// click selects it), and for a constant of the open file the span of its
/// stored value (editable in place).
#[derive(Clone)]
pub(super) struct Chip {
    label: String,
    widget: Option<usize>,
    constant: Option<usize>,
    /// The colour a fixed colour is, editable or not: shown as a swatch.
    rgb: Option<[u8; 3]>,
    /// Game data (a view model or its field), drawn as the graph draws its
    /// view-model nodes, apart from the widgets' properties.
    model: bool,
}

/// The PropertyType slot `s` holds by where it is defined: a container's
/// input or a component's property by its template (`defined(widget,
/// component, uid)`), else what the file itself says (built-ins, its own
/// properties, game data fields). One axis of anything is a number (round 6
/// refused a Float onto a container's SizeIn.x, as the game's files do
/// 1,017 times). `FileTable::kind` and the golden checks both ask this.
pub(super) fn defined_kind(
    asset: &Asset,
    slots: &[Slot],
    s: usize,
    defined: &dyn Fn(usize, Option<u32>, u32) -> Option<u32>,
) -> Option<u32> {
    let by_definition = match slots.get(s)? {
        Slot::Widget {
            widget: Some(w),
            path,
        } => match path[..] {
            [(field, uid)] if uid & 0x8000_0000 == 0 => {
                defined(*w, None, uid).map(|k| if field >= 2 { T_FLOAT } else { k })
            }
            [(_, c), (field, uid)] if c & 0x8000_0000 == 0 => {
                defined(*w, Some(c), uid).map(|k| if field >= 2 { T_FLOAT } else { k })
            }
            _ => None,
        },
        _ => None,
    };
    by_definition.or_else(|| slot_kind(asset, slots, s))
}

/// The PropertyType a slot holds, when the file tells it: constants by
/// the kind stored with them (by their value when none is), template
/// properties by their kind, built-in widget properties by what they are,
/// game data by its field. Container inputs and component properties live
/// in other files: unknown here.
pub(super) fn slot_kind(asset: &Asset, slots: &[Slot], s: usize) -> Option<u32> {
    Some(match slots.get(s)? {
        Slot::Constant { kind, value } => kind.or_else(|| value_kind(value))?,
        Slot::Template(_, field) if *field >= 2 => T_FLOAT,
        Slot::Template(p, _) => asset.properties().get(*p)?.kind,
        Slot::Widget { path, .. } => match path[..] {
            [(field, id)] if id & 0x8000_0000 != 0 => match id & 0x7fff_ffff {
                builtin::VISIBLE => T_BOOL,
                // One channel of a colour is a number, as an axis is.
                builtin::COLOR if field >= 2 => T_FLOAT,
                builtin::COLOR => T_COLOR,
                builtin::ALPHA | builtin::ROTATION => T_FLOAT,
                builtin::TEXT | builtin::IMAGE => T_STRING,
                builtin::POSITION | builtin::SCALE | builtin::PIVOT if field >= 2 => T_FLOAT,
                builtin::SIZE if field >= 2 => T_FLOAT,
                builtin::POSITION | builtin::SCALE | builtin::PIVOT => T_VEC3,
                builtin::SIZE => T_VEC2,
                _ => return None,
            },
            // One axis of a field (Layout.TextPlane.x) is a number.
            [_, .., (field, _)] if field >= 2 => T_FLOAT,
            [_, .., (_, last)] if path.len() >= 2 => anno_formats::phx::data_field_type(last)?,
            _ => return None,
        },
        _ => return None,
    })
}

/// The PropertyType a value is.
pub(super) fn value_kind(v: &DefaultValue) -> Option<u32> {
    Some(match v {
        DefaultValue::Bool(_) => T_BOOL,
        DefaultValue::F32(_) => T_FLOAT,
        DefaultValue::S32(_) => 1,
        DefaultValue::U32(_) => 2,
        DefaultValue::Str(_) => T_STRING,
        DefaultValue::Vec(v) if v.len() == 2 => T_VEC2,
        DefaultValue::Vec(v) if v.len() == 3 => T_VEC3,
        _ => return None,
    })
}

/// `UiEditor::file_table`'s last result: the file and layout generation it
/// was built for.
pub(super) type TableCache = ((Option<Guid>, u64), u64, std::sync::Arc<FileTable>);

/// The bindings of one file as the editor shows them: see
/// [`UiEditor::file_table`]. The Bindings pane lists them, the graph draws
/// them.
pub(super) struct FileTable {
    /// The template (None: the open file) and the instance key of the copy
    /// whose traces `eval` holds.
    pub file: (Option<Guid>, u64),
    pub asset: std::sync::Arc<Asset>,
    pub widgets: std::sync::Arc<[Widget]>,
    pub table: anno_formats::phx::binding::Bindings,
    inputs: std::collections::HashMap<usize, std::sync::Arc<Asset>>,
    /// Components (and effects) the slots address, by (widget, component
    /// uid): the instance's name and its ComponentTemplate, which names
    /// their properties.
    components: std::collections::HashMap<(usize, u32), (String, std::sync::Arc<Asset>)>,
    /// Native properties of a container's template root the slots address
    /// by one id, by (widget, uid): a button's OnMouseEnter comes from its
    /// WidgetType, not from the template's inputs. Its name, kind
    /// (OnMouseEnter is an Event) and the widget type that defines it.
    native: std::collections::HashMap<(usize, u32), (String, u32, Option<Guid>)>,
    /// Slots inside nested templates: each step's name, the widget it walks
    /// into or at the end a clip there.
    nested: std::collections::HashMap<usize, Vec<String>>,
    /// The slots that are events, one bit each (see `resolve_events`).
    events: Vec<u64>,
    pub eval: Option<super::scene::FileEval>,
}

impl FileTable {
    /// Slot `slot` in words: "Glow · AlphaTint", "-Size", "15".
    pub fn label(&self, slot: usize) -> String {
        let input = |w: usize, path: &[(u32, u32)]| match *path {
            [(_, uid)] => {
                let t = self.inputs.get(&w);
                t.and_then(|t| {
                    let p = t.properties().iter().find(|p| p.uid == uid)?;
                    Some(t.property_name(p).to_owned())
                })
                .or_else(|| self.native.get(&(w, uid)).map(|n| n.0.clone()))
            }
            [(_, c), (_, uid)] => {
                if let Some((name, t)) = self.components.get(&(w, c)) {
                    let p = t.properties().iter().find(|p| p.uid == uid)?;
                    return Some(format!("{name}.{}", t.property_name(p)));
                }
                // A field of an object input (`-BtnEvents`, `Layout`): the
                // input by name, the field by the game's name for it where
                // the field table has one (`Layout.Overflow_Y`; round 6
                // showed `Layout.20517da9`), else by id.
                let t = self.inputs.get(&w)?;
                let p = t.properties().iter().find(|p| p.uid == c)?;
                let field = anno_formats::phx::data_field_name(uid)
                    .map_or_else(|| format!("{uid:08x}"), str::to_owned);
                Some(format!("{}.{field}", t.property_name(p)))
            }
            _ => None,
        };
        let steps = self.nested.get(&slot).map_or(&[][..], Vec::as_slice);
        slot_label(
            &self.asset,
            &self.widgets,
            &self.table.slots,
            slot,
            &input,
            steps,
        )
    }

    /// The PropertyType slot `s` holds: a component's or container's
    /// property by its definition (Icon3D.IconType is an enum: round 5
    /// wired it into X), else what the file itself says (built-ins, its
    /// own properties, game data fields).
    pub fn kind(&self, s: usize) -> Option<u32> {
        let defined = |w: usize, component: Option<u32>, uid: u32| {
            let t = match component {
                // A container's input, else its root's WidgetType's own
                // property (a button's OnPress is an Event).
                None => {
                    return self
                        .inputs
                        .get(&w)
                        .and_then(|t| t.properties().iter().find(|p| p.uid == uid))
                        .map(|p| p.kind)
                        .or_else(|| self.native.get(&(w, uid)).map(|n| n.1));
                }
                Some(c) => &self.components.get(&(w, c))?.1,
            };
            t.properties().iter().find(|p| p.uid == uid).map(|p| p.kind)
        };
        defined_kind(&self.asset, &self.table.slots, s, &defined)
    }

    /// What slot `s` is for, in the words of the game's Phoenix sources: a
    /// widget type's own property (a button's OnPress: "Dispatched when
    /// the player presses…") or a component's.
    pub fn description(&self, s: usize) -> Option<String> {
        let Slot::Widget {
            widget: Some(w),
            path,
        } = self.table.slots.get(s)?
        else {
            return None;
        };
        match path[..] {
            [(_, uid)] => {
                let owner = self.native.get(&(*w, uid))?.2?;
                anno_formats::phx::property_description(owner, uid)
            }
            [(_, c), (_, uid)] if c & 0x8000_0000 == 0 => {
                let owner = self.components.get(&(*w, c))?.1.guid()?;
                anno_formats::phx::property_description(owner, uid)
            }
            _ => None,
        }
    }

    /// The events container widget `w`'s widget type declares (a button's
    /// OnPress, OnMouseEnter), by name: (uid, name).
    pub fn type_events(&self, w: usize) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = self
            .native
            .iter()
            .filter(|((x, _), (_, kind, _))| *x == w && *kind == T_EVENT)
            .map(|((_, uid), (name, ..))| (*uid, name.clone()))
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// The template container widget `w` shows, when the file has one.
    pub fn template(&self, w: usize) -> Option<&Asset> {
        self.inputs.get(&w).map(|t| &**t)
    }

    /// The view model slot `s` is or reads into: (the widget whose inputs
    /// hold it, 0 for this file's own, the object property's uid). A view
    /// model is the game data a screen or a container is handed: an object
    /// input (PropertyType 0x13, typed by its GameObjectTemplate: `-Data`,
    /// GameContext, a button's BtnEvents). Its fields are the steps after
    /// it. Everything else (built-ins, components, value inputs) is a
    /// property. Round 17 (user): the two read alike everywhere.
    pub fn view_model(&self, s: usize) -> Option<(usize, u32)> {
        let object = |w: usize, uid: u32| {
            let props = match w {
                0 => self.asset.properties(),
                w => self.template(w).map_or(&[][..], |t| t.properties()),
            };
            props
                .iter()
                .any(|p| p.uid == uid && p.object_asset.is_some())
        };
        match self.table.slots.get(s)? {
            Slot::Template(p, _) => {
                let p = self.asset.properties().get(*p)?;
                p.object_asset.is_some().then_some((0, p.uid))
            }
            Slot::Widget { widget, path } => {
                let w = widget.unwrap_or(0);
                let &(_, uid) = path.first()?;
                // A step into an object walks a view model's fields, unless
                // it is the widget's component (AnnoButtonWT · Focus).
                let walks = path.len() > 1
                    && uid & 0x8000_0000 == 0
                    && !self.components.contains_key(&(w, uid));
                (walks || object(w, uid)).then_some((w, uid))
            }
            _ => None,
        }
    }

    /// A view model's name: its property's, without the input dash
    /// ("Data", "BtnEvents").
    pub fn view_model_name(&self, w: usize, uid: u32) -> String {
        let named = |a: &Asset| {
            a.properties()
                .iter()
                .find(|p| p.uid == uid)
                .map(|p| a.property_name(p).trim_start_matches('-').to_owned())
        };
        match w {
            0 => named(&self.asset),
            w => self.template(w).and_then(named),
        }
        .unwrap_or_else(|| format!("{uid:08x}"))
    }

    /// The EnumAsset slot `s` holds an entry of, when known: a template
    /// or container input's or a component property's own, or for a
    /// label's alignment, overflow and sizing the shipped enum of that
    /// name.
    pub fn enum_of(&self, s: usize) -> Option<Guid> {
        let own = |t: &Asset, uid: u32| {
            t.properties()
                .iter()
                .find(|p| p.uid == uid)
                .and_then(|p| p.enum_asset)
                .map(|r| r.id)
        };
        match self.table.slots.get(s)? {
            Slot::Template(p, 1) => self.asset.properties().get(*p)?.enum_asset.map(|r| r.id),
            Slot::Widget {
                widget: Some(w),
                path,
            } => match path[..] {
                // The alignment and overflow enums are named after their
                // property; both sizings share one.
                [(1, id)] if id & 0x8000_0000 != 0 => {
                    let id = id & 0x7fff_ffff;
                    let name = if builtin::SIZING.contains(&id) {
                        "TextBoxSizing"
                    } else if builtin::ALIGNMENT.contains(&id) || builtin::OVERFLOW.contains(&id) {
                        builtin::name(id)?
                    } else {
                        return None;
                    };
                    anno_formats::phx::enum_named(name)
                }
                [(1, uid)] => own(self.inputs.get(w)?, uid),
                [(_, c), (1, uid)] if c & 0x8000_0000 == 0 => {
                    own(&self.components.get(&(*w, c))?.1, uid)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether slot `s` is an event or an action (a press, a clip's
    /// Completed or Start, a widget being shown): something that happens,
    /// holding no value. A place of no kind of its own is one when what it
    /// meets says so (`resolve_events`); None when nothing does.
    pub fn event_slot(&self, s: usize) -> Option<bool> {
        if self
            .events
            .get(s / 64)
            .is_some_and(|w| w >> (s % 64) & 1 != 0)
        {
            return Some(true);
        }
        self.own_event(s)
    }

    /// Whether slot `s` is an event by its own kind; None when it has none
    /// known (an object's fields, a widget type's own properties).
    fn own_event(&self, s: usize) -> Option<bool> {
        match self.table.slots.get(s)? {
            Slot::Clip { .. } => return Some(true),
            Slot::Constant { .. } => return Some(false),
            // A widget's or a nested template's ("BtnOverlay_DS › · Start")
            // builtin: Shown, and 0x3d beside it (copied only into clip
            // actions); an animated template's Start … Completed.
            Slot::Widget { path, .. } | Slot::Nested { path, .. } => {
                if let Some(&(_, id)) = path.last()
                    && id & 0x8000_0000 != 0
                {
                    let b = id & 0x7fff_ffff;
                    if b == builtin::SHOWN
                        || b == 0x3d
                        || (builtin::START..=builtin::COMPLETED).contains(&b)
                    {
                        return Some(true);
                    }
                }
            }
            _ => {}
        }
        self.kind(s).map(|k| k == T_EVENT || k == T_ACTION)
    }

    /// What binding `i` carries: per source, whether it is an event, and
    /// whether what it writes is one. Data and events meet only through
    /// the converters made for it: FalseToTrueChange, TrueToFalseChange
    /// and ValueChange send an event when a value changes; EventToTrue and
    /// EventToFalse set a yes/no when one arrives; EventAssign writes its
    /// value (every first of a pair) when its event (every second)
    /// arrives. A copy or a pass if / unless / if-else carries whatever its
    /// values are, an event when either end says so (an object's field
    /// has no kind of its own).
    pub fn flow(&self, i: usize) -> Flow {
        self.flow_by(i, &|s| self.event_slot(s))
    }

    /// `flow` with the slots' event-ness from `event`.
    fn flow_by(&self, i: usize, event: &dyn Fn(usize) -> Option<bool>) -> Flow {
        let b = &self.table.bindings[i];
        let ins: Vec<Option<bool>> = b.sources.iter().map(|&s| event(s)).collect();
        let out = b
            .targets
            .iter()
            .filter_map(|&t| event(t))
            .reduce(|a, b| a || b);
        let n = ins.len();
        let named = |f: &dyn Fn(usize) -> bool| (0..n).map(f).collect::<Vec<bool>>();
        match self.converter(i).as_str() {
            "FalseToTrueChange" | "TrueToFalseChange" | "ValueChange" | "EnumToEvent" => Flow {
                args: vec![false; n],
                out: true,
            },
            "EventToTrue" | "EventToFalse" | "EventToBool" | "EventToBoolChange" => Flow {
                args: vec![true; n],
                out: false,
            },
            "EventAssign" => Flow {
                args: named(&|k| k % 2 == 1),
                out: false,
            },
            name @ ("Assign" | "TwoWayAssign" | "IfTrueAssign" | "IfFalseAssign"
            | "IfTrueFalseAssign") => {
                // The values it passes on; the rest is its condition.
                let carried = |k: usize| match name {
                    "IfTrueAssign" | "IfFalseAssign" => k == 0,
                    "IfTrueFalseAssign" => k < 2,
                    _ => true,
                };
                let event = out == Some(true) || (0..n).any(|k| carried(k) && ins[k] == Some(true));
                Flow {
                    args: named(&|k| {
                        if carried(k) {
                            event
                        } else {
                            ins[k] == Some(true)
                        }
                    }),
                    out: event,
                }
            }
            _ => Flow {
                args: ins.iter().map(|e| *e == Some(true)).collect(),
                out: out == Some(true),
            },
        }
    }

    /// The slots that are events, one bit each. A place of no kind of its
    /// own (an object's field, a result kept somewhere) is one when it meets
    /// one: copied from or into an event, written by a converter that sends
    /// one, read where a converter takes one. Carried along copies and gates
    /// both ways until nothing changes; a gate's condition stays a value.
    /// Drawing, the List and the wire verdicts all read this (round 16:
    /// BtnEvents.Leave, only ever copied into clip Leave · Start, was drawn
    /// orange yet dropped onto a pass if's condition).
    fn resolve_events(&self) -> Vec<u64> {
        let n = self.table.slots.len();
        let own: Vec<Option<bool>> = (0..n).map(|s| self.own_event(s)).collect();
        let has = |bits: &[u64], s: usize| bits[s / 64] >> (s % 64) & 1 != 0;
        let mut bits = vec![0u64; n.div_ceil(64)];
        for s in (0..n).filter(|&s| own[s] == Some(true)) {
            bits[s / 64] |= 1 << (s % 64);
        }
        // Each pass that changes anything marks one more slot.
        for _ in 0..=n {
            let mut more = Vec::new();
            for (i, b) in self.table.bindings.iter().enumerate() {
                let f = self.flow_by(i, &|s| {
                    if s < n && has(&bits, s) {
                        Some(true)
                    } else {
                        own.get(s).copied().flatten()
                    }
                });
                let sources = b.sources.iter().zip(&f.args).filter(|(_, e)| **e);
                let targets = b.targets.iter().filter(|_| f.out);
                more.extend(sources.map(|(&s, _)| s).chain(targets.copied()));
            }
            more.retain(|&s| s < n && own[s].is_none() && !has(&bits, s));
            if more.is_empty() {
                break;
            }
            for s in more {
                bits[s / 64] |= 1 << (s % 64);
            }
        }
        bits
    }

    /// Binding `i`'s converter by name.
    pub fn converter(&self, i: usize) -> String {
        let c = self.table.bindings[i].converter;
        anno_formats::phx::binding::converter_name(c)
            .map(str::to_owned)
            .or_else(|| Converter::of(c).map(|c| format!("{c:?}")))
            .unwrap_or_else(|| format!("{c:08x}"))
    }

    /// Binding `i`'s converter as the graph, the List and messages name it,
    /// and its inputs (see [`logic_name`]).
    pub fn logic(&self, i: usize) -> (String, Vec<String>) {
        logic_name(&self.converter(i), self.table.bindings[i].sources.len())
    }

    /// The last layout's run of binding `i`.
    pub fn trace(&self, i: usize) -> Option<anno_formats::phx::binding::Trace> {
        self.eval.as_ref()?.trace.get(i)
    }

    /// Whether binding `i` ran in the last layout, and "(args) → value" or
    /// why it did not, in words.
    pub fn outcome(&self, i: usize) -> (bool, String) {
        let (b, table, trace) = (&self.table.bindings[i], &self.table, self.trace(i));
        let inputs = &self.inputs;
        let ran = trace.is_some();
        let text = match trace {
            // An event holds no value: what a run left there means nothing
            // (an event copy read "(false) → false", round 15).
            Some(_) if self.flow(i).out => "passes its event on".into(),
            Some(t) => format!(
                "({}) → {}",
                t.args.iter().map(short).collect::<Vec<_>>().join(", "),
                t.value.as_ref().map_or_else(|| "nothing".to_owned(), short)
            ),
            // Clip Start/Stop are written when an event fires;
            // Completed is read when the clip sends it.
            None if b
                .sources
                .iter()
                .chain(&b.targets)
                .any(|&s| matches!(table.slots.get(s), Some(Slot::Clip { .. }))) =>
            {
                "waits for an event: drives a clip".into()
            }
            // Game data handed to a container's template whole:
            // the layout passes the object's fields along (see
            // `Asset::data_feeds`), so there is no one value.
            None if Converter::of(b.converter) == Some(Converter::Assign)
                && matches!(
                    b.sources.first().and_then(|&s| table.slots.get(s)),
                    Some(Slot::Widget {
                        widget: Some(0) | None,
                        ..
                    })
                )
                && b.targets.iter().any(|&t| {
                    // The container's input itself, holding an object, not
                    // one field of it.
                    let Some(Slot::Widget {
                        widget: Some(w),
                        path,
                    }) = table.slots.get(t)
                    else {
                        return false;
                    };
                    let [(_, uid)] = path[..] else { return false };
                    inputs.get(w).is_some_and(|tpl| {
                        tpl.properties()
                            .iter()
                            .any(|p| p.uid == uid && p.object_asset.is_some())
                    })
                }) =>
            {
                "hands the game object down: its fields go along".into()
            }
            // Events happen while the game runs; a layout has none to run
            // them with (round 15: these read "a source has no value").
            None if self.flow(i).args.iter().any(|&e| e) => "waits for its event".into(),
            None if self.flow(i).out => "sends an event when its value changes".into(),
            None if Converter::of(b.converter).is_none() => {
                match anno_formats::phx::binding::converter_name(b.converter) {
                    Some("GetCount") => "not evaluated: counts a list".into(),
                    Some(_) => "not evaluated: fires on an event".into(),
                    None => "not evaluated: converter unknown".into(),
                }
            }
            None if matches!(
                Converter::of(b.converter),
                Some(Converter::EventToTrue | Converter::EventToFalse)
            ) =>
            {
                "not evaluated: fires on input or a value change".into()
            }
            None if b.mode > 1 => "not evaluated: runs on events".into(),
            None => "not run: a source has no value".into(),
        };
        (ran, text)
    }
}

/// What a binding carries in and out (`FileTable::flow`).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Flow {
    /// Per source: an event.
    pub args: Vec<bool>,
    /// What it writes is an event.
    pub out: bool,
}

/// One binding as the pane shows it.
pub(super) struct View {
    /// Index into the file's bindings.
    index: usize,
    /// One of its targets is the widget.
    writes: bool,
    converter: String,
    /// The converter as the file names it.
    raw: String,
    /// It carries an event somewhere (in or out), not only values.
    event: bool,
    sources: Vec<Chip>,
    targets: Vec<Chip>,
    /// "args → value" of the last run, or why it did not run.
    result: String,
    ran: bool,
}

/// What the pane shows for one file: its bindings and its template
/// property values (name, value).
pub(super) struct FileBindings {
    /// The template, or None for the open file.
    file: Option<Guid>,
    views: Vec<View>,
    values: Vec<(String, String)>,
}

pub(super) fn slot_widget(slots: &[Slot], slot: usize) -> Option<usize> {
    match slots.get(slot)? {
        Slot::Widget { widget, .. } => *widget,
        Slot::Nested { widget, .. } => Some(*widget),
        _ => None,
    }
}

impl UiEditor {
    /// The node the Bindings pane is about: the selection, else the file
    /// being edited (the template's top widget while one is edited in
    /// place, not the screen around it).
    pub(super) fn subject(&self) -> usize {
        self.canvas
            .selected
            .or_else(|| {
                self.scope?;
                (0..self.nodes.len()).find(|&i| !self.in_scope.is_empty() && self.in_scope.get(i))
            })
            .unwrap_or(0)
    }

    /// The bindings of the file being edited as the pane shows them: its
    /// slots are `self.asset`'s, named as the graph names them. None while
    /// the pane is about a template not edited here.
    pub(super) fn edited_table(&mut self) -> Option<std::sync::Arc<FileTable>> {
        let ft = self.file_table(self.subject())?;
        self.is_edited_file(ft.file.0).then_some(ft)
    }

    /// The file a canvas node comes from and its instance key (see
    /// [`scene::file_eval`](super::scene::file_eval)): its innermost
    /// template, or the open file (key 0).
    pub(super) fn node_file(&self, node: usize) -> (Option<Guid>, u64) {
        self.nodes
            .get(node)
            .and_then(|n| n.owners.first())
            .map_or((None, 0), |&(g, inst)| (Some(g), inst))
    }

    /// Selects the canvas node showing `widget` of the file instance.
    pub(super) fn select_in_file(&mut self, file: (Option<Guid>, u64), widget: usize) {
        let found = (0..self.nodes.len())
            .find(|&n| self.nodes[n].widget == widget && self.node_file(n) == file);
        if let Some(n) = found {
            self.select_widget(n);
        }
    }

    /// The bindings of the file canvas node `node` comes from (this file,
    /// or the template it is part of), with the last layout's traces for
    /// that copy of it.
    pub(super) fn file_table(&mut self, node: usize) -> Option<std::sync::Arc<FileTable>> {
        let file = self.node_file(node);
        if let Some((f, generation, t)) = &self.table_cache
            && *f == file
            && *generation == self.layout_gen
        {
            return Some(t.clone());
        }
        // The screen is the layout root: while a template is edited in
        // place, `self.asset` is that template, not the screen.
        let asset = match file.0 {
            None => std::sync::Arc::new(self.layout_root().clone()),
            Some(g) => self.images.asset(&self.install, g)?,
        };
        let widgets = asset.widgets();
        let table = asset.bindings(&widgets);
        // Container inputs by name, from each container's template.
        let mut inputs: std::collections::HashMap<usize, std::sync::Arc<Asset>> =
            Default::default();
        for (w, wd) in widgets.iter().enumerate() {
            if let Some(r) = asset.container_template(wd)
                && let Some(t) = self.images.asset(&self.install, r.id)
            {
                inputs.insert(w, t);
            }
        }
        // Only the widgets whose components a slot names: listing a
        // widget's components walks the whole file.
        let mut components = std::collections::HashMap::new();
        let named: std::collections::BTreeSet<usize> = table
            .slots
            .iter()
            .filter_map(|s| match s {
                Slot::Widget {
                    widget: Some(w),
                    path,
                } if matches!(path[..], [(_, c), _] if c & 0x8000_0000 == 0) => Some(*w),
                _ => None,
            })
            .collect();
        for w in named {
            let Some(wd) = widgets.get(w) else { continue };
            // A container's paths also reach its template root's
            // components (AnnoButtonWT · Focus.IsFocused).
            let root = inputs
                .get(&w)
                .and_then(|t| {
                    let tw = t.widgets();
                    let r = tw.first()?;
                    Some(
                        t.components(r)
                            .into_iter()
                            .chain(t.effects(r))
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_default();
            for c in asset
                .components(wd)
                .into_iter()
                .chain(asset.effects(wd))
                .chain(root)
            {
                if let Some(t) = c.template.and_then(|g| self.images.asset(&self.install, g)) {
                    components.insert((w, c.uid), (c.name, t));
                }
            }
        }
        // A container's native properties: those of its template root's
        // WidgetType and that type's parents (ButtonWT → … → OnMouseEnter)
        // that are not the template's own inputs, the nearest type's first.
        // Slots name them by one id; their events (OnPress) are what a wire
        // can start from before any binding reads them.
        let mut native = std::collections::HashMap::new();
        for (&w, t) in &inputs {
            let mut at = t.clone();
            // Chains are three or four deep; the bound stops a cycle.
            for _ in 0..8 {
                let Some(r) = at.widget_type() else { break };
                let Some(wt) = self.images.asset(&self.install, r.id) else {
                    break;
                };
                for p in wt.properties() {
                    if !t.properties().iter().any(|q| q.uid == p.uid) {
                        native
                            .entry((w, p.uid))
                            .or_insert_with(|| (wt.property_name(p).to_owned(), p.kind, wt.guid()));
                    }
                }
                at = wt;
            }
        }
        // The steps of a slot inside nested templates, by name.
        let mut nested = std::collections::HashMap::new();
        for (s, slot) in table.slots.iter().enumerate() {
            let Slot::Nested { widget, inner, .. } = slot else {
                continue;
            };
            let Some(at) = inputs.get(widget).cloned() else {
                continue;
            };
            let (images, install) = (&mut self.images, &self.install);
            nested.insert(
                s,
                nested_steps(at, inner, &mut |g| images.asset(install, g)),
            );
        }
        let eval = super::scene::file_eval(&self.images, self.layout_root(), file.1);
        let mut ft = FileTable {
            file,
            asset,
            widgets,
            table,
            inputs,
            components,
            native,
            nested,
            events: Vec::new(),
            eval,
        };
        ft.events = ft.resolve_events();
        let t = std::sync::Arc::new(ft);
        self.table_cache = Some((file, self.layout_gen, t.clone()));
        Some(t)
    }

    /// The selected node's file, its bindings that touch the widget (all of
    /// them with `bindings_all`), and its template property values.
    pub(super) fn binding_views(&mut self, node: usize) -> Option<FileBindings> {
        let widget = self.nodes.get(node)?.widget;
        let ft = self.file_table(node)?;
        let (file, asset, table) = (ft.file, &ft.asset, &ft.table);
        let eval = ft.eval.as_ref();
        let touches = |b: &anno_formats::phx::binding::Binding| {
            b.sources
                .iter()
                .chain(&b.targets)
                .any(|&s| slot_widget(&table.slots, s) == Some(widget))
        };
        let views = table
            .bindings
            .iter()
            .enumerate()
            // Nothing selected: the whole file, as the graph shows it.
            .filter(|(_, b)| self.bindings_all || self.canvas.selected.is_none() || touches(b))
            .map(|(i, b)| {
                let label = |s: &usize| Chip {
                    label: ft.label(*s),
                    widget: slot_widget(&table.slots, *s),
                    constant: table
                        .constants
                        .get(s)
                        .copied()
                        .filter(|_| self.is_edited_file(file.0)),
                    rgb: table.constants.get(s).and_then(|&sp| {
                        let kind = match table.slots.get(*s) {
                            Some(Slot::Constant { kind, .. }) => *kind,
                            _ => None,
                        };
                        super::constant_rgb(asset, sp, kind)
                    }),
                    model: ft.view_model(*s).is_some(),
                };
                let (ran, result) = ft.outcome(i);
                let flow = ft.flow(i);
                View {
                    index: i,
                    writes: b
                        .targets
                        .iter()
                        .any(|&t| slot_widget(&table.slots, t) == Some(widget)),
                    // In the graph's words ("pass unless", "when it turns
                    // true"), the file's name in the tooltip (round 14:
                    // the List said IfFalseAssign where the graph said
                    // pass unless).
                    converter: ft.logic(i).0,
                    raw: ft.converter(i),
                    event: flow.out || flow.args.iter().any(|&e| e),
                    sources: b.sources.iter().map(label).collect(),
                    targets: b.targets.iter().map(label).collect(),
                    ran,
                    result,
                }
            })
            .collect();
        let mut values: Vec<(String, String)> = asset
            .properties()
            .iter()
            .map(|p| {
                let v = eval
                    .and_then(|e| e.value(p.uid).cloned())
                    .unwrap_or_else(|| asset.default_at(p.default));
                (asset.property_name(p).to_owned(), short(&v))
            })
            .collect();
        values.sort();
        Some(FileBindings {
            file: file.0,
            views,
            values,
        })
    }

    /// What feeds each property of `node`'s widget that a binding writes,
    /// by property (`Size.y`): "follows Bg_Alpha" for a copy, "computed
    /// from A, B" when a converter works it out (a gate pair is two
    /// bindings on one property: their sources merge).
    pub(super) fn follows(&mut self, node: usize) -> Vec<(String, String)> {
        let Some(widget) = self.nodes.get(node).map(|n| n.widget) else {
            return Vec::new();
        };
        let Some(ft) = self.file_table(node) else {
            return Vec::new();
        };
        let table = &ft.table;
        let mut by: std::collections::BTreeMap<String, (bool, Vec<String>)> = Default::default();
        for b in &table.bindings {
            for &t in &b.targets {
                if !matches!(table.slots.get(t), Some(Slot::Widget { widget: Some(w), .. }) if *w == widget)
                {
                    continue;
                }
                let label = ft.label(t);
                let prop = label.rsplit(" · ").next().unwrap_or(&label).to_owned();
                let (computed, from) = by.entry(prop).or_default();
                *computed |= b.converter != ASSIGN;
                for &s in &b.sources {
                    let s = plain(ft.label(s));
                    if !from.contains(&s) {
                        from.push(s);
                    }
                }
            }
        }
        by.into_iter()
            .map(|(p, (computed, from))| {
                let verb = if computed { "computed from" } else { "follows" };
                (p, format!("{verb} {}", from.join(", ")))
            })
            .collect()
    }
}

impl Editor {
    /// The inspector's Evaluated block for the selected widget, from this
    /// file or a template: what the layout drew (box, visibility, opacity,
    /// text) and every value a binding or clip put over the stored one.
    pub(super) fn evaluated_block(&self) -> Option<Div> {
        let ui = self.ui.as_ref()?;
        let n = ui.canvas.selected?;
        let node = ui.nodes.get(n)?;
        let b = ui.frames.get(n)?.bound;
        let v3 = |v: [f32; 3]| format!("{:.1}, {:.1}", v[0], v[1]);
        let mut block = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section("Evaluated"))
            .child(info_row(
                "Shown",
                format!(
                    "{} · opacity {:.2}{}",
                    if node.visible { "visible" } else { "hidden" },
                    node.opacity,
                    if node.runtime {
                        " · placed by the game"
                    } else {
                        ""
                    }
                ),
            ));
        if let Some(t) = &node.text {
            block = block.child(info_row("Text", t.clone()));
        }
        if let Some([x, y, w, h]) = node.clip {
            block = block.child(info_row(
                "Masked to",
                format!("{x:.0}, {y:.0} · {w:.0}×{h:.0}"),
            ));
        }
        let rows = [
            ("Position", b.position.map(v3)),
            ("Pivot", b.pivot.map(v3)),
            ("Rotation", b.rotation.map(|r| format!("{r:.1}°"))),
            ("Scale", b.scale.map(v3)),
            ("Size", b.size.map(|s| format!("{:.1} × {:.1}", s[0], s[1]))),
            ("AlphaTint", b.alpha.map(|a| format!("{a:.2}"))),
            ("Visible", b.visible.map(|v| v.to_string())),
            (
                "ColorTint",
                b.tint.map(|c| format!("{}, {}, {}", c[0], c[1], c[2])),
            ),
        ];
        let mut any = false;
        for (name, value) in rows {
            if let Some(v) = value {
                any = true;
                block = block.child(info_row(format!("{name} (bound)"), v));
            }
        }
        if !any {
            block = block.child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("No binding or clip overrides a stored value here"),
            );
        }
        Some(block)
    }
}

impl Editor {
    pub fn render_ui_bindings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(ui) = &mut self.ui else {
            return empty_pane("No UI asset open");
        };
        let node = ui.subject();
        let all = ui.bindings_all;
        let graph_on = ui.graph.on;
        let pick = ui.graph.pick.clone();
        let file = ui.node_file(node);
        // The graph draws from its own cache: the list's rows (a label for
        // every binding) are not built just to be thrown away.
        let (guid, views, values, count) = if graph_on {
            let Some(t) = ui.file_table(node) else {
                return empty_pane("Select a widget");
            };
            let widget = ui.nodes.get(node).map(|n| n.widget);
            let whole = all || ui.canvas.selected.is_none();
            let count = t
                .table
                .bindings
                .iter()
                .filter(|b| {
                    whole
                        || b.sources
                            .iter()
                            .chain(&b.targets)
                            .any(|&s| slot_widget(&t.table.slots, s) == widget)
                })
                .count();
            (file.0, Vec::new(), Vec::new(), count)
        } else {
            let Some(FileBindings {
                file: guid,
                views,
                values,
            }) = ui.binding_views(node)
            else {
                return empty_pane("Select a widget");
            };
            let count = views.len();
            (guid, views, values, count)
        };
        let file_name = match guid {
            None => self
                .ui_find
                .display_name(&ui.path)
                .unwrap_or_else(|| ui.path.rsplit('/').next().unwrap_or(&ui.path).to_owned()),
            Some(g) => {
                let g = g.to_string();
                self.ui_find
                    .index
                    .as_ref()
                    .and_then(|i| i.entries.iter().find(|e| e.file == g))
                    .map_or_else(|| format!("template {}", &g[..8]), |e| e.name.clone())
            }
        };
        let Some(ui) = &mut self.ui else {
            return empty_pane("No UI asset open");
        };
        // Nothing selected: the file is the subject.
        let widget_name = match ui.canvas.selected {
            Some(_) => ui
                .nodes
                .get(node)
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            None => "Everything".into(),
        };
        let editing = ui.editing.as_ref().map(|e| (e.field, e.display()));
        let chip = |c: &Chip, id: String, cx: &mut Context<Self>| {
            // A colour shows as one; editable, its swatch opens the picker.
            let swatch = c.rgb.map(|rgb3| {
                super::graph::view::color_swatch(
                    SharedString::from(format!("{id}-swatch")),
                    rgb3,
                    12.,
                    c.constant,
                    cx,
                )
            });
            if let Some(span) = c.constant {
                let shown = editing
                    .as_ref()
                    .filter(|e| e.0 == span)
                    .map(|e| e.1.clone());
                let field = text_field(SharedString::from(id), &c.label, shown, "")
                    .w(px(90.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_span_edit(span, super::Keys::Canvas, Filter::Text, window, cx)
                    }));
                return div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .children(swatch)
                    .child(field)
                    .into_any_element();
            }
            let el = div()
                .id(SharedString::from(id))
                .px_1()
                .rounded(px(2.))
                .bg(if c.model {
                    crate::colors::get().graph.data[0]
                } else {
                    theme::get().border
                })
                .flex()
                .items_center()
                .gap_1()
                .children(swatch)
                .child(c.label.clone());
            match c.widget {
                Some(w) => el
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::get().highlight))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.select_in_file(file, w);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
                None => el.into_any_element(),
            }
        };
        let mut list = div().flex().flex_col().gap(px(4.)).text_size(px(11.));
        if views.is_empty() {
            list = list.child(
                div()
                    .text_color(theme::get().muted)
                    .child("No binding reads or writes this widget"),
            );
        }
        for (i, v) in views.iter().enumerate() {
            let mut sources = div().flex().flex_wrap().gap_1();
            for (k, s) in v.sources.iter().enumerate() {
                sources = sources.child(chip(s, format!("bs-{i}-{k}"), cx));
            }
            let mut targets = div().flex().flex_wrap().gap_1();
            for (k, t) in v.targets.iter().enumerate() {
                targets = targets.child(chip(t, format!("bt-{i}-{k}"), cx));
            }
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p_1()
                    .border_b_1()
                    .border_color(theme::get().border)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id(("ui-binding-converter", v.index))
                                    // The graph's colours: values blue,
                                    // events orange.
                                    .text_color(if v.event {
                                        crate::colors::get().graph.event
                                    } else {
                                        theme::get().accent
                                    })
                                    .child(v.converter.clone())
                                    .tooltip(Tooltip::text(
                                        v.raw.clone(),
                                        "",
                                        if v.event {
                                            "Carries an event: something that happens, holding no value"
                                        } else {
                                            "Carries a value"
                                        },
                                    )),
                            )
                            .child(sources)
                            .child(
                                div()
                                    .when(v.event, |d| d.text_color(crate::colors::get().graph.event))
                                    .child("→"),
                            )
                            .child(targets),
                    )
                    .child(
                        div()
                            .text_color(if v.ran { theme::get().text } else { theme::get().muted })
                            .child(v.result.clone()),
                    ),
            );
        }
        // What the template's inputs and outputs hold after the bindings ran.
        let mut props = div().flex().flex_col().text_size(px(11.)).child(
            div()
                .pb_1()
                .text_color(theme::get().muted)
                .font_weight(FontWeight::SEMIBOLD)
                .child(if values.is_empty() {
                    "Template values: none".to_owned()
                } else {
                    "Template values".to_owned()
                }),
        );
        for (name, value) in &values {
            props = props.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .overflow_hidden()
                            .text_color(theme::get().muted)
                            .child(name.clone()),
                    )
                    .child(div().overflow_hidden().child(value.clone())),
            );
        }
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .text_size(px(11.))
            .child(format!("{widget_name} in {file_name} · {count} binding(s)"))
            .child(
                button(if graph_on { "Graph" } else { "List" }, graph_on)
                    .tooltip(Tooltip::text(
                        "View",
                        "",
                        "Show the bindings as a list, or as a graph of what feeds what with the values on the wires. In the graph, drag from a pin on a node's right onto a pin on another's left to wire them; right-click a wired pin or a logic node to remove it",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.graph.on = !ui.graph.on;
                        }
                        cx.notify();
                    })),
            )
            .when(graph_on && !all, |d| {
                let feeds = self.ui.as_ref().is_some_and(|u| u.graph.feeds);
                d.child(
                    button("What it feeds", feeds)
                        .tooltip(Tooltip::text(
                            "What it feeds",
                            "",
                            "Also show everything the widget's values reach: what changes if you change it",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                ui.graph.feeds = !ui.graph.feeds;
                                ui.graph.fitted = None;
                            }
                            cx.notify();
                        })),
                )
            })
            .when(graph_on, |d| {
                d.child(
                    button("Fit", false)
                        .tooltip(Tooltip::text(
                            "Fit",
                            "",
                            "Show the whole graph again. Drag to pan, wheel to zoom",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                ui.graph.fitted = None;
                                ui.graph.overview = true;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    button("Tidy", false)
                        .tooltip(Tooltip::text(
                            "Tidy",
                            "",
                            "Put dragged nodes back where the layout places them",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                ui.graph.moved.retain(|k, _| k.0 != file.0);
                                ui.graph.let_go(file.0);
                            }
                            cx.notify();
                        })),
                )
            })
            .child(
                button(if all { "All in file" } else { "This widget" }, all)
                    .tooltip(Tooltip::text(
                        "Scope",
                        "",
                        "Show the bindings that touch the selected widget, or every binding of its file",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.bindings_all = !ui.bindings_all;
                        }
                        cx.notify();
                    })),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(panel_header("Bindings"))
            .child(header)
            // The colour picker, over the graph or the List.
            .children(pick.map(|p| {
                let canvas_hidden = !self.docks[self.dock_index()]
                    .visible_panes()
                    .contains(&crate::editor::Pane::UiCanvas);
                super::graph::view::color_popover(&p, canvas_hidden, cx)
            }))
            .child(if graph_on {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_binding_graph(cx))
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_2()
                    .p_2()
                    .child(
                        vscroll(
                            "ui-bindings-list",
                            &self.scrolls.get("ui-bindings-list"),
                            list,
                        )
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(
                        vscroll(
                            "ui-bindings-props",
                            &self.scrolls.get("ui-bindings-props"),
                            props,
                        )
                        .w(px(320.))
                        .flex_none(),
                    )
            })
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Binding authoring: the inspector's Data section lists what feeds the
// selected widget and binds one of its properties to game data, a template
// input or a value, through a few common converters.

/// Game data shipped templates bind, by game-object class (an
/// ObjectTemplate guid): the field paths known to exist on each, with ids
/// that are right for it, and where an object at a path is handed whole to
/// a template input of another class, that class's paths below it. Built
/// once per game from every template.
pub struct Catalog {
    by_class: std::collections::HashMap<Guid, Vec<Vec<u32>>>,
    /// (class, path) → the class of the object there.
    links: std::collections::HashMap<Guid, Vec<(Vec<u32>, Guid)>>,
}

/// Levels of hand-off a catalog path follows, and how many paths a class
/// lists at most: enough to reach a screen's panels from its root.
const LINK_DEPTH: usize = 3;
const MAX_PATHS: usize = 4000;

impl Catalog {
    pub fn build(install: &anno_formats::vfs::Install) -> Catalog {
        use rayon::prelude::*;
        type Found = (
            Vec<(Guid, Vec<u32>)>,
            Vec<(Guid, Vec<u32>, Guid, u32)>,
            Option<(Guid, Vec<(u32, Guid)>)>,
        );
        let files = install.list("data/ui/studio/generated");
        let found: Vec<Found> = files
            .par_iter()
            .filter_map(|f| {
                let a = Asset::parse(install.read(f).ok()?).ok()?;
                let w = a.widgets();
                // The template's object inputs by uid, for the hand-offs
                // into it.
                let inputs = a.guid().map(|g| {
                    (
                        g,
                        a.properties()
                            .iter()
                            .filter_map(|p| Some((p.uid, p.object_asset?.id)))
                            .collect(),
                    )
                });
                Some((a.data_paths(&w), a.data_feeds(&w), inputs))
            })
            .collect();
        let mut by_class: std::collections::HashMap<Guid, Vec<Vec<u32>>> = Default::default();
        let mut inputs: std::collections::HashMap<Guid, Vec<(u32, Guid)>> = Default::default();
        let mut feeds = Vec::new();
        for (paths, f, i) in found {
            for (g, p) in paths {
                let list = by_class.entry(g).or_default();
                if !list.contains(&p) {
                    list.push(p);
                }
            }
            feeds.extend(f);
            inputs.extend(i);
        }
        let mut links: std::collections::HashMap<Guid, Vec<(Vec<u32>, Guid)>> = Default::default();
        for (class, path, template, uid) in feeds {
            let Some(&(_, to)) = inputs
                .get(&template)
                .and_then(|i| i.iter().find(|i| i.0 == uid))
            else {
                continue;
            };
            let list = links.entry(class).or_default();
            if to != class && !list.contains(&(path.clone(), to)) {
                list.push((path, to));
            }
        }
        for list in by_class.values_mut() {
            list.sort_by_cached_key(|p| field_path(p));
        }
        Catalog { by_class, links }
    }

    /// The process's catalog for `install`'s game, built on first use.
    pub fn of(install: &anno_formats::vfs::Install) -> std::sync::Arc<Catalog> {
        static BUILT: std::sync::Mutex<Vec<(anno_formats::game::Game, std::sync::Arc<Catalog>)>> =
            std::sync::Mutex::new(Vec::new());
        let game = install.game();
        if let Some(c) = BUILT.lock().unwrap().iter().find(|(g, _)| *g == game) {
            return c.1.clone();
        }
        let c = std::sync::Arc::new(Catalog::build(install));
        BUILT.lock().unwrap().push((game, c.clone()));
        c
    }

    /// Field paths known on `class`, its own first, then those reached
    /// through the objects it hands on, sorted by name.
    pub fn paths(&self, class: Guid) -> Vec<Vec<u32>> {
        let mut out: Vec<Vec<u32>> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        self.walk(class, &mut Vec::new(), LINK_DEPTH, &mut out, &mut seen);
        out.sort_by_cached_key(|p| field_path(p));
        out
    }

    fn walk(
        &self,
        class: Guid,
        prefix: &mut Vec<u32>,
        depth: usize,
        out: &mut Vec<Vec<u32>>,
        seen: &mut std::collections::HashSet<Vec<u32>>,
    ) {
        for p in self.by_class.get(&class).into_iter().flatten() {
            if out.len() >= MAX_PATHS {
                return;
            }
            let full: Vec<u32> = prefix.iter().chain(p).copied().collect();
            if seen.insert(full.clone()) {
                out.push(full);
            }
        }
        if depth == 0 {
            return;
        }
        for (p, to) in self.links.get(&class).into_iter().flatten() {
            let len = prefix.len();
            prefix.extend(p);
            self.walk(*to, prefix, depth - 1, out, seen);
            prefix.truncate(len);
        }
    }
}

/// `Data.TextData.Text`: field ids by name, unknown ones by id.
fn field_path(ids: &[u32]) -> String {
    ids.iter()
        .map(|&id| {
            anno_formats::phx::data_field_name(id)
                .map_or_else(|| format!("{id:08x}"), str::to_owned)
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// PropertyType `k` by name ("Float", "Boolean"), else by number.
pub(super) fn kind_name(k: u32) -> String {
    anno_formats::phx::property_type_name(k).map_or_else(|| format!("type {k}"), str::to_owned)
}

// PropertyType codes (`phx::property_type_name`).
pub(super) const T_BOOL: u32 = 0x00;
/// Something that happens (a press, a clip's Completed), not a value.
pub(super) const T_EVENT: u32 = 0x09;
/// Something done when an event arrives (a clip's Start).
pub(super) const T_ACTION: u32 = 0x0a;
pub(super) const T_FLOAT: u32 = 0x03;
pub(super) const T_STRING: u32 = 0x04;
pub(super) const T_VEC2: u32 = 0x05;
pub(super) const T_VEC3: u32 = 0x06;
/// An enum: its values are entry ids (UInt32).
pub(super) const T_ENUM: u32 = 0x11;
pub(super) const T_COLOR: u32 = 0x12;
/// A game object, a view model (`FileTable::view_model`).
pub(super) const T_OBJECT: u32 = 0x13;

/// A widget property the binder writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Visible,
    Alpha,
    Color,
    Text,
    Image,
    X,
    Y,
    Width,
    Height,
    ScaleX,
    ScaleY,
    Rotation,
}

impl Target {
    /// What shipped screens bind most (round 14: Size and ColorTint, bound
    /// everywhere in the game's files, could only be rewired, never bound
    /// anew).
    pub(super) const ALL: [Target; 12] = [
        Target::Visible,
        Target::Alpha,
        Target::Color,
        Target::Text,
        Target::Image,
        Target::X,
        Target::Y,
        Target::Width,
        Target::Height,
        Target::ScaleX,
        Target::ScaleY,
        Target::Rotation,
    ];

    /// The property's name as the file, the graph and Data name it, one
    /// name everywhere (round 9: the binder said Opacity and X where the
    /// pins said AlphaTint and Position.x).
    pub(super) fn label(self) -> String {
        let [(field, id)] = self.path()[..] else {
            unreachable!("binder targets are one built-in")
        };
        let name = builtin::name(id & 0x7fff_ffff).expect("binder targets have names");
        format!("{name}{}", axis(field))
    }

    /// What it is called in other tools, where the file's name says less.
    fn alias(self) -> Option<&'static str> {
        match self {
            Target::Alpha => Some("Opacity"),
            Target::Color => Some("Colour, Tint"),
            Target::X => Some("X"),
            Target::Y => Some("Y"),
            Target::Width => Some("Width"),
            Target::Height => Some("Height"),
            Target::ScaleX => Some("Scale X"),
            Target::ScaleY => Some("Scale Y"),
            Target::Rotation => Some("Rotation"),
            Target::Visible | Target::Text | Target::Image => None,
        }
    }

    /// PropertyField and built-in id (an axis of a vector for X/Y/Scale).
    pub(super) fn path(self) -> Vec<(u32, u32)> {
        let b = |f: u32, id: u32| vec![(f, 0x8000_0000 | id)];
        match self {
            Target::Visible => b(1, builtin::VISIBLE),
            Target::Alpha => b(1, builtin::ALPHA),
            Target::Color => b(1, builtin::COLOR),
            Target::Text => b(1, builtin::TEXT),
            Target::Image => b(1, builtin::IMAGE),
            Target::X => b(2, builtin::POSITION),
            Target::Y => b(3, builtin::POSITION),
            Target::Width => b(2, builtin::SIZE),
            Target::Height => b(3, builtin::SIZE),
            Target::ScaleX => b(2, builtin::SCALE),
            Target::ScaleY => b(3, builtin::SCALE),
            Target::Rotation => b(1, builtin::ROTATION),
        }
    }

    /// The PropertyType the property takes.
    pub(super) fn kind(self) -> u32 {
        match self {
            Target::Visible => T_BOOL,
            Target::Text | Target::Image => T_STRING,
            Target::Color => T_COLOR,
            _ => T_FLOAT,
        }
    }

    /// Whether a widget of `kind` has the property.
    pub(super) fn fits(self, kind: super::canvas::Kind) -> bool {
        use super::canvas::Kind;
        match self {
            Target::Text => kind == Kind::Label,
            Target::Image => kind == Kind::Image,
            _ => true,
        }
    }
}

/// Where the bound value comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A field path under one of the file's object-typed template
    /// properties (index into [`Asset::properties`]).
    Data { prop: usize, fields: Vec<u32> },
    /// One of the file's own template properties.
    Input(usize),
    /// A fixed value, typed as the target takes it.
    Value,
}

/// What happens between source and target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    AsIs,
    /// LogicalInvert: a yes/no turned around.
    Invert,
    /// IsEmpty: a text or list with nothing in it.
    IsEmpty,
    /// Equal against the value typed in.
    Equals,
    /// ValueToString: a number shown as text.
    AsText,
}

impl Transform {
    /// The transforms that make sense for a property.
    fn for_target(t: Target) -> &'static [Transform] {
        match t.kind() {
            T_BOOL => &[
                Transform::AsIs,
                Transform::Invert,
                Transform::IsEmpty,
                Transform::Equals,
            ],
            T_STRING => &[Transform::AsIs, Transform::AsText],
            _ => &[Transform::AsIs],
        }
    }

    /// Whether this transform for target `t` takes a source of `kind`: as
    /// is by the graph's rule for copies (what the game's own files copy,
    /// `graph::wiring::fits`), the others by what their converter reads. Equals
    /// compares with a value typed as the source is, so it takes the kinds
    /// a fixed value can be typed as; an enum by its entry id, as the
    /// shipped files compare one (18 times; round 7 refused it here while
    /// the graph took it).
    fn accepts(self, t: Target, kind: u32) -> bool {
        match self {
            Transform::AsIs => super::graph::wiring::fits(kind, t.kind()),
            Transform::Invert => kind == T_BOOL,
            Transform::IsEmpty => kind == T_STRING,
            Transform::Equals => matches!(kind, T_BOOL | 1 | 2 | T_FLOAT | T_STRING | T_ENUM),
            Transform::AsText => matches!(kind, 1 | 2 | T_FLOAT),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Transform::AsIs => "as is",
            Transform::Invert => "inverted",
            Transform::IsEmpty => "is empty",
            Transform::Equals => "equals",
            Transform::AsText => "as text",
        }
    }

    fn converter(self) -> u32 {
        match self {
            Transform::AsIs => ASSIGN,
            Transform::Invert => name_crc(b"LogicalInvertConverterInstance"),
            Transform::IsEmpty => name_crc(b"IsEmptyConverterInstance"),
            Transform::Equals => name_crc(b"EqualConverterInstance"),
            Transform::AsText => name_crc(b"ValueToStringConverterInstance"),
        }
    }
}

/// What Bind will do (see `UiEditor::bind_plan`): the binding, the plain
/// copies it replaces (binding indices, ascending), and the words.
pub(super) struct BindPlan {
    binding: anno_formats::phx::binding::NewBinding,
    replaces: Vec<usize>,
    /// A fixed tint: its constant is a copied ColorType, set to this.
    tint: Option<[u8; 3]>,
    pub said: String,
}

/// The binder's state while a binding is being put together.
pub struct Binder {
    pub target: Target,
    pub source: Option<Source>,
    pub transform: Transform,
    /// Narrows the game-data list.
    pub search: String,
    /// Lists fields of every type, not only those the property takes.
    pub all_types: bool,
    /// The value for [`Source::Value`] and [`Transform::Equals`].
    pub value: String,
    /// Which of the two is being typed: 0 search, 1 value.
    pub edit: Option<TextEdit<u8>>,
    /// What Bind would do, for the choices and file revision it was worked
    /// out for: worked out again only when they change (every render did,
    /// doubling the inspector's cost while the binder was open).
    plan: Option<(PlanKey, Result<BindPlan, String>)>,
}

/// What a `BindPlan` depends on: the file's revision, the widget, and the
/// choices.
type PlanKey = (
    u64,
    Option<usize>,
    Target,
    Option<Source>,
    Transform,
    String,
);

impl Binder {
    fn new() -> Binder {
        Binder {
            // What modders bind first: showing and hiding.
            target: Target::Visible,
            source: None,
            transform: Transform::AsIs,
            search: String::new(),
            all_types: false,
            value: String::new(),
            edit: None,
            plan: None,
        }
    }
}

/// `text` as a value of PropertyType `kind`. NaN and infinities are
/// refused: typed into Opacity, NaN hid the widget and went into the file.
fn typed_value(kind: u32, text: &str) -> anyhow::Result<DefaultValue> {
    let t = text.trim();
    Ok(match kind {
        T_BOOL => DefaultValue::Bool(match t {
            "1" | "true" | "yes" | "on" => 1,
            "0" | "false" | "no" | "off" => 0,
            _ => anyhow::bail!("Type on or off"),
        }),
        T_FLOAT => {
            let v = t
                .parse::<f32>()
                .map_err(|_| anyhow::anyhow!("Type a number"))?;
            anyhow::ensure!(v.is_finite(), "Type a finite number, not {t}");
            DefaultValue::F32(v.to_bits())
        }
        1 => DefaultValue::S32(
            t.parse()
                .map_err(|_| anyhow::anyhow!("Type a whole number"))?,
        ),
        2 => DefaultValue::U32(
            t.parse()
                .map_err(|_| anyhow::anyhow!("Type a whole number, 0 or more"))?,
        ),
        T_ENUM => DefaultValue::U32(
            t.parse()
                .map_err(|_| anyhow::anyhow!("Type the entry's id, a whole number"))?,
        ),
        _ => DefaultValue::Str(text.to_owned()),
    })
}

impl UiEditor {
    /// Opens the binder on the selected widget of this file.
    pub(super) fn open_binder(&mut self) {
        if self.canvas.selected.is_some_and(|n| !self.nodes[n].foreign) {
            self.binder = Some(Binder::new());
        }
    }

    /// The data a field path in the binder reads: (template property index,
    /// its name, the paths the install binds on its class).
    pub(super) fn data_roots(&self, catalog: &Catalog) -> Vec<(usize, String, Vec<Vec<u32>>)> {
        self.asset
            .properties()
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let class = p.object_asset?.id;
                Some((
                    i,
                    self.asset.property_name(p).to_owned(),
                    catalog.paths(class),
                ))
            })
            .collect()
    }

    /// What Bind would do with the binder as it stands, or why not: every
    /// check `bind` makes, without changing anything, so the binder can
    /// say it under the button before the click (round 5 found the reason
    /// only after it). The binding, the copies it replaces, and the words.
    pub(super) fn bind_plan(&mut self) -> Result<BindPlan, String> {
        use anno_formats::phx::binding::{NewBinding, NewSlot};
        self.ensure_editable().map_err(|e| e.to_string())?;
        let node = self
            .canvas
            .selected
            .filter(|&n| !self.nodes[n].foreign)
            .ok_or("Select a widget of this file")?;
        // Names as the graph shows them (a container's events, nested
        // clips), not the file's ids.
        let ft = self.edited_table().ok_or("no bindings here")?;
        let b = self.binder.as_ref().ok_or("Nothing to bind")?;
        let source = b
            .source
            .clone()
            .ok_or("Pick where the value comes from first")?;
        // A fixed value turned or compared is just another fixed value, and
        // its constant would be typed for the property, not the transform
        // (equals compared true with "true" and hid the minimap).
        if source == Source::Value && b.transform != Transform::AsIs {
            return Err(format!(
                "A fixed value is taken as is: type the value {} should get",
                b.target.label()
            ));
        }
        // The graph's rule here too: a source of a kind the property (as
        // transformed) does not take is refused, Bool into Opacity no less
        // than text. A field of unknown type passes.
        let kind = match &source {
            Source::Data { fields, .. } => fields
                .last()
                .and_then(|&f| anno_formats::phx::data_field_type(f)),
            Source::Input(i) => self.asset.properties().get(*i).map(|p| p.kind),
            Source::Value => None,
        };
        if let Some(k) = kind
            && !b.transform.accepts(b.target, k)
        {
            return Err(match b.transform {
                Transform::AsIs => format!(
                    "{} takes {}, and that source is {}",
                    b.target.label(),
                    kind_name(b.target.kind()),
                    kind_name(k)
                ),
                t => format!(
                    "{} {} cannot take {}",
                    b.target.label(),
                    t.label(),
                    kind_name(k)
                ),
            });
        }
        let widget = self.nodes[node].widget;
        let typed = |kind: u32| typed_value(kind, &b.value).map_err(|e| format!("{e:#}"));
        let first = match &source {
            Source::Data { prop, fields } => {
                let uid = self.asset.properties()[*prop].uid;
                NewSlot::Widget {
                    widget: 0,
                    path: std::iter::once((1, uid))
                        .chain(fields.iter().map(|&f| (1, f)))
                        .collect(),
                }
            }
            Source::Input(i) => NewSlot::Template(*i, 1),
            // A fixed tint as the game's files keep one: a ColorType object
            // (a plain one copied, its colour set after), never a bare
            // Vector3 of kind Color, which no shipped file has (round 15).
            Source::Value if b.target.kind() == T_COLOR => NewSlot::Copied(
                self.colour_constant()
                    .ok_or("No shipped fixed colour to start from")?,
            ),
            Source::Value => NewSlot::Constant {
                kind: b.target.kind(),
                value: typed(b.target.kind())?,
            },
        };
        let tint = match (&source, b.target.kind()) {
            (Source::Value, T_COLOR) => {
                Some(super::parse_rgb(&b.value).map_err(|e| format!("{e:#}"))?)
            }
            _ => None,
        };
        let mut sources = vec![first];
        if b.transform == Transform::Equals {
            // Compared as the source is typed: a field's type, else text.
            let kind = match &source {
                Source::Data { fields, .. } => fields
                    .last()
                    .and_then(|&f| anno_formats::phx::data_field_type(f))
                    .unwrap_or(T_STRING),
                Source::Input(i) => self.asset.properties()[*i].kind,
                Source::Value => unreachable!("refused above"),
            };
            sources.push(NewSlot::Constant {
                kind,
                value: typed(kind)?,
            });
        }
        // Who writes the property now. An axis and its whole vector are one
        // place: X on a Position set whole stacked a second writer (round
        // 5); one the whole is set by refuses, plain copies of the same or
        // a narrower place are replaced, a converter refuses.
        let widgets = self.asset.widgets();
        let table = self.asset.bindings(&widgets);
        let [(field, id)] = b.target.path()[..] else {
            unreachable!("binder targets are one built-in")
        };
        let place = |x: usize| match &table.slots[x] {
            Slot::Widget {
                widget: Some(w),
                path,
            } if *w == widget => match path[..] {
                [(f, i)] if i == id => Some(f),
                _ => None,
            },
            _ => None,
        };
        let label = |s: usize| plain(ft.label(s));
        for bd in &table.bindings {
            if let Some(&t) = bd
                .targets
                .iter()
                .find(|&&t| place(t) == Some(1) && field != 1)
            {
                let by = bd.sources.first().map_or_else(String::new, |&s| label(s));
                return Err(format!(
                    "{} is set whole by {by}; remove that binding first",
                    label(t).rsplit(" · ").next().unwrap_or_default()
                ));
            }
        }
        let replaces = super::graph::wiring::writers(&table, |x| {
            place(x).is_some_and(|f| f == field || field == 1)
        })?;
        // A template input the property already feeds would read itself.
        if let Source::Input(i) = source {
            let at = (0..table.slots.len()).filter(|&x| place(x).is_some());
            let fed = super::graph::wiring::Places::new(&table.slots, &table.bindings)
                .readers(&table.bindings, at);
            let reads_itself = fed.into_iter().any(|r| {
                table.bindings[r]
                    .targets
                    .iter()
                    .any(|&t| matches!(table.slots[t], Slot::Template(p, _) if p == i))
            });
            if reads_itself {
                return Err(format!(
                    "{} would read itself: it already follows {}",
                    plain(
                        self.asset
                            .property_name(&self.asset.properties()[i])
                            .to_owned()
                    ),
                    b.target.label()
                ));
            }
        }
        let what = format!("{} of {}", b.target.label(), self.nodes[node].name);
        let said = match replaces.first() {
            Some(&w) => format!("{what}, replacing {}", label(table.bindings[w].sources[0])),
            None => what,
        };
        Ok(BindPlan {
            tint,
            binding: NewBinding {
                converter: b.transform.converter(),
                sources,
                targets: vec![NewSlot::Widget {
                    widget,
                    path: b.target.path(),
                }],
            },
            replaces,
            said,
        })
    }

    /// Works out what Bind would do when the binder's choices or the file
    /// changed since it last did (see `Binder::plan`).
    pub(super) fn refresh_bind_plan(&mut self) {
        let Some(b) = &self.binder else { return };
        let key = (
            self.revision,
            self.canvas.selected,
            b.target,
            b.source.clone(),
            b.transform,
            b.value.clone(),
        );
        if b.plan.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let plan = self.bind_plan();
        if let Some(b) = &mut self.binder {
            b.plan = Some((key, plan));
        }
    }

    /// Adds the binder's binding to the selected widget as one undo step
    /// (see `bind_plan`, which says what it does or why not).
    /// A plain fixed colour to copy for a new fixed tint: a ColorType
    /// constant with its own colour (no palette entry), from this file
    /// when it has one, else from the first shipped file that does.
    fn colour_constant(&self) -> Option<Vec<u8>> {
        let own = |a: &anno_formats::phx::Asset| {
            let table = a.bindings(&a.widgets());
            table.slots.iter().enumerate().find_map(|(s, slot)| {
                let Slot::Constant {
                    kind: Some(T_COLOR),
                    ..
                } = slot
                else {
                    return None;
                };
                let &span = table.constants.get(&s)?;
                super::constant_colour(a, span)?;
                a.constant_bytes(span)
            })
        };
        own(&self.asset).or_else(|| {
            self.install
                .list(super::GENERATED.trim_end_matches('/'))
                .iter()
                .find_map(|p| {
                    own(&anno_formats::phx::Asset::parse(self.install.read(p).ok()?).ok()?)
                })
        })
    }

    pub(super) fn bind(&mut self) -> anyhow::Result<String> {
        let plan = self.bind_plan().map_err(anyhow::Error::msg)?;
        let mut next = self.asset.clone();
        // The later first, so earlier indices hold.
        for &w in plan.replaces.iter().rev() {
            next = next.remove_binding(&next.widgets(), w)?;
        }
        let mut next = next.add_binding(&next.widgets(), &plan.binding)?;
        // The copied tint takes the typed colour (the new binding is the
        // last; its first source the constant).
        if let Some(rgb) = plan.tint {
            let table = next.bindings(&next.widgets());
            let span = table
                .bindings
                .last()
                .and_then(|b| b.sources.first())
                .and_then(|s| table.constants.get(s))
                .and_then(|&sp| super::constant_colour(&next, sp))
                .ok_or_else(|| anyhow::anyhow!("the new tint's colour is not its own"))?;
            next = next.apply(&super::Edit::Color { span, rgb })?.asset;
        }
        self.binder = None;
        self.replace_asset(next);
        Ok(format!("Bound {}", plan.said))
    }

    /// Removes bindings `indices` of this file as one undo step (a gate
    /// pair the graph shows as one node is two).
    pub(super) fn unbind(&mut self, indices: &[usize]) -> anyhow::Result<String> {
        self.ensure_editable()?;
        let mut order = indices.to_vec();
        // The later first, so the earlier keep their index.
        order.sort_unstable_by(|a, b| b.cmp(a));
        order.dedup();
        // Said before they go: what they read and what they set.
        let widgets = self.asset.widgets();
        let table = self.asset.bindings(&widgets);
        anyhow::ensure!(
            order.iter().all(|&i| i < table.bindings.len()),
            "that binding is gone already (undone or changed); nothing removed"
        );
        // Names as the graph shows them: "BtnOverlay_DS › clip Enter ·
        // Stop", a container's OnMouseLeave (R16O5 read "› ·" and ids).
        let ft = self
            .edited_table()
            .ok_or_else(|| anyhow::anyhow!("these bindings are read only here"))?;
        let label = |s: &usize| plain(ft.label(*s));
        let said = match &order[..] {
            [i] => {
                let b = &table.bindings[*i];
                format!(
                    "Removed {} → {}",
                    b.sources.iter().map(label).collect::<Vec<_>>().join(", "),
                    b.targets.iter().map(label).collect::<Vec<_>>().join(", ")
                )
            }
            many => format!(
                "Removed {} bindings setting {}",
                many.len(),
                many.iter()
                    .flat_map(|&i| table.bindings[i].targets.iter().map(label))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let mut next = self.asset.clone();
        for i in &order {
            next = next.remove_binding(&next.widgets(), *i)?;
        }
        self.replace_asset(next);
        Ok(said)
    }

    /// Typing into the binder; false when it is not being typed into.
    pub(super) fn binder_key(&mut self, event: &KeyDownEvent) -> bool {
        use gpui_editor_kit::field::EditEvent;
        let Some(b) = &mut self.binder else {
            return false;
        };
        let Some(edit) = &mut b.edit else {
            return false;
        };
        let e = edit.key_down(event);
        let text = edit.text.clone();
        match edit.field {
            0 => b.search = text,
            _ => b.value = text,
        }
        match e {
            EditEvent::Commit | EditEvent::Cancel => b.edit = None,
            EditEvent::Ignored if event.keystroke.modifiers.control => return false,
            _ => {}
        }
        true
    }
}

impl Editor {
    /// Reads the game-data catalog in the background, once per game.
    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = &self.ui else {
            return;
        };
        if ui.catalog.is_some() {
            return;
        }
        let install = ui.install.clone();
        cx.spawn(async move |this, cx| {
            let catalog = cx
                .background_executor()
                .spawn(async move { Catalog::of(&install) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.catalog = Some(catalog);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The inspector's Data section for the selected widget of this file:
    /// the bindings that write it (each removable), then the binder.
    pub(super) fn data_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let ui = self.ui.as_mut()?;
        let node = ui.canvas.selected.filter(|&n| n > 0)?;
        // A template's widget: what feeds it, read only, and the way in.
        let own = !ui.nodes[node].foreign;
        let views = ui.binding_views(node).map(|f| f.views).unwrap_or_default();
        ui.refresh_bind_plan();
        let ui = self.ui.as_ref()?;
        let (heading, open) = gpui_editor_kit::widgets::collapsible(
            "ui-data",
            format!(
                "Data · {}",
                match views.iter().filter(|v| v.writes).count() {
                    0 => "not bound".to_owned(),
                    n => crate::project::plural(n, "binding", "bindings"),
                }
            ),
            true,
            window,
            cx,
        );
        let mut d = div().flex().flex_col().gap_1().child(heading);
        if !open {
            return Some(d);
        }
        for v in views.iter().filter(|v| v.writes) {
            let index = v.index;
            let what: Vec<String> = v
                .targets
                .iter()
                .filter(|c| c.widget == Some(ui.nodes[node].widget))
                .map(|c| plain(c.label.rsplit(" · ").next().unwrap_or(&c.label).to_owned()))
                .collect();
            let from: Vec<String> = v.sources.iter().map(|c| plain(c.label.clone())).collect();
            // The row truncates; the whole sentence is one hover away.
            let full = format!("{} ← {}", what.join(", "), from.join(", "));
            // Removing takes the whole binding: name what else it sets.
            let others: Vec<String> = v
                .targets
                .iter()
                .filter(|c| c.widget != Some(ui.nodes[node].widget))
                .map(|c| c.label.clone())
                .collect();
            let remove_tip = if others.is_empty() {
                "Remove this binding".to_owned()
            } else {
                format!("Remove this binding; it also sets {}", others.join(", "))
            };
            d = d.child(
                div()
                    .id(("ui-data-binding", index))
                    .tooltip(Tooltip::text(full, "", ""))
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .rounded(px(3.))
                    .bg(theme::get().input)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(11.))
                            .child(div().text_color(theme::get().accent).child(what.join(", ")))
                            .child("←")
                            .child(div().flex_1().min_w_0().truncate().child(from.join(", ")))
                            .when(own, |d| {
                                d.child(
                                    button("×", false)
                                        .tooltip(Tooltip::text(remove_tip, "", ""))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(ui) = &mut this.ui {
                                                let r = ui.unbind(&[index]);
                                                this.ui_tree_result(r);
                                            }
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::get().muted)
                            .child(format!("{} · {}", v.converter, v.result)),
                    ),
            );
        }
        if !own {
            let at = ui.nodes[node].owners.first().copied();
            d = d.child(hint_line(if views.iter().any(|v| v.writes) {
                "Set by the template it is part of. Edit the template here to change or add bindings."
            } else {
                "Part of a template. Edit the template here to bind its properties."
            }));
            if let Some((g, key)) = at {
                d = d.child(
                    button("Edit the template here", false).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                match ui.enter_template(g, key) {
                                    Ok(m) => ui.message = Some((m, false)),
                                    Err(e) => ui.message = Some((format!("{e:#}"), true)),
                                }
                            }
                            cx.notify();
                        },
                    )),
                );
            }
            return Some(d);
        }
        let Some(b) = &ui.binder else {
            return Some(
                d.child(button("Bind a property…", false).on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.open_binder();
                        }
                        this.load_catalog(cx);
                        cx.notify();
                    },
                ))),
            );
        };
        let kind = ui.nodes[node].kind;
        // 1. The property.
        let mut targets = div().flex().flex_wrap().gap_1();
        for t in Target::ALL.into_iter().filter(|t| t.fits(kind)) {
            let chip = button(t.label(), b.target == t);
            let chip = match t.alias() {
                Some(alias) => chip.tooltip(Tooltip::text(alias, "", "")),
                None => chip,
            };
            targets = targets.child(chip.on_click(cx.listener(move |this, _, _, cx| {
                this.keys_to(super::Keys::Canvas);
                if let Some(b) = this.ui.as_mut().and_then(|u| u.binder.as_mut()) {
                    b.target = t;
                    if !Transform::for_target(t).contains(&b.transform) {
                        b.transform = Transform::AsIs;
                    }
                }
                cx.notify();
            })));
        }
        d = d
            .child(section("Bind"))
            .child(hint_line("Property"))
            .child(targets)
            .child(hint_line("Comes from"));
        // 2. Where the value comes from: game data under the file's object
        // properties, the file's own inputs, or a fixed value.
        let catalog = ui.catalog.clone();
        let editing = |field: u8| {
            b.edit
                .as_ref()
                .filter(|e| e.field == field)
                .map(|e| e.display())
        };
        d = d.child(
            text_field(
                "ui-bind-search",
                &b.search,
                editing(0),
                "Search game data and inputs",
            )
            .on_click(type_into(0, b.search.clone(), cx)),
        );
        let needle = b.search.to_lowercase();
        // A fixed value first: it is a kind of source, not one of the
        // catalogue's, and after sixty rows it read as missing (round 14).
        let mut list = div().flex().flex_col().child(
            div()
                .id("ui-bind-value")
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .h(px(20.))
                .rounded(px(2.))
                .cursor_pointer()
                .text_size(px(11.))
                .hover(|s| s.bg(theme::get().hover))
                .when(b.source == Some(Source::Value), |s| {
                    s.bg(theme::get().selected)
                })
                .child("A fixed value")
                .on_click(pick_source(Source::Value, cx)),
        );
        let mut shown = 0;
        let mut hidden = 0;
        match &catalog {
            None => {
                list = list.child(hint_line("Reading what game data the install binds…"));
            }
            Some(catalog) => {
                // Leaf first: three `GameContext.SceneData.MapData…` rows
                // cut at the inspector's width all read the same. A leaf
                // the search names exactly ranks first.
                let mut rows = Vec::new();
                for (prop, name, paths) in ui.data_roots(catalog) {
                    for fields in paths {
                        let path = field_path(&fields);
                        let leaf = fields
                            .last()
                            .and_then(|&f| anno_formats::phx::data_field_type(f));
                        if !needle.is_empty() && !path.to_lowercase().contains(&needle) {
                            continue;
                        }
                        // What the property takes, unless asked for all.
                        // Fields of unknown type stay, marked `?`.
                        if !b.all_types && leaf.is_some_and(|k| !b.transform.accepts(b.target, k)) {
                            hidden += 1;
                            continue;
                        }
                        let (parent, last) = path.rsplit_once('.').unwrap_or(("", &path));
                        let rank = match last.to_lowercase() {
                            l if !needle.is_empty() && l == needle => 0,
                            l if !needle.is_empty() && l.contains(&needle) => 1,
                            _ => 2,
                        };
                        let within = if parent.is_empty() {
                            name.clone()
                        } else {
                            format!("{name}.{parent}")
                        };
                        rows.push((
                            rank,
                            last.to_owned(),
                            within,
                            prop,
                            fields,
                            leaf,
                            path.clone(),
                        ));
                    }
                }
                rows.sort_by_key(|r| r.0);
                shown = rows.len();
                for (_, last, within, prop, fields, leaf, path) in rows.into_iter().take(60) {
                    let src = Source::Data { prop, fields };
                    let picked = b.source.as_ref() == Some(&src);
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("ui-bind-data-{prop}-{path}")))
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_1()
                            .h(px(20.))
                            .rounded(px(2.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .hover(|s| s.bg(theme::get().hover))
                            .when(picked, |s| s.bg(theme::get().selected))
                            .tooltip(Tooltip::text(path, "", ""))
                            .child(div().flex_none().child(last))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(theme::get().muted)
                                    .child(format!("in {within}")),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.))
                                    .text_color(theme::get().muted)
                                    .child(
                                        leaf.and_then(anno_formats::phx::property_type_name)
                                            .unwrap_or("?"),
                                    ),
                            )
                            .on_click(pick_source(src, cx)),
                    );
                }
            }
        }
        for (i, p) in ui.asset.properties().iter().enumerate() {
            let name = ui.asset.property_name(p).to_owned();
            if p.object_asset.is_some()
                || (!needle.is_empty() && !name.to_lowercase().contains(&needle))
            {
                continue;
            }
            shown += 1;
            if shown > 60 {
                continue;
            }
            let src = Source::Input(i);
            let picked = b.source.as_ref() == Some(&src);
            list = list.child(
                div()
                    .id(("ui-bind-input", i))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .h(px(20.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .hover(|s| s.bg(theme::get().hover))
                    .when(picked, |s| s.bg(theme::get().selected))
                    .child(div().text_color(theme::get().muted).child("input"))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .on_click(pick_source(src, cx)),
            );
        }
        if shown > 60 {
            list = list.child(hint_line(format!("{} more; search to narrow", shown - 60)));
        }
        if hidden > 0 || b.all_types {
            let all = b.all_types;
            list = list.child(
                div()
                    .id("ui-bind-all-types")
                    .px_1()
                    .py(px(2.))
                    .cursor_pointer()
                    .text_size(px(10.))
                    .text_color(theme::get().accent)
                    .hover(|s| s.underline())
                    .child(if all {
                        "Only fields this property takes".to_owned()
                    } else {
                        format!("{hidden} more of other or unknown types")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(b) = this.ui.as_mut().and_then(|u| u.binder.as_mut()) {
                            b.all_types = !all;
                        }
                        cx.notify();
                    })),
            );
        }
        d = d.child(
            gpui_editor_kit::widgets::vscroll(
                "ui-bind-list",
                &self.scrolls.get("ui-bind-list"),
                list,
            )
            .h(px(220.))
            .border_1()
            .border_color(theme::get().border)
            .rounded(px(3.)),
        );
        // 3. In between.
        let mut transforms = div().flex().flex_wrap().gap_1();
        let offered = if b.source == Some(Source::Value) {
            &[Transform::AsIs][..]
        } else {
            Transform::for_target(b.target)
        };
        for &t in offered {
            transforms = transforms.child(button(t.label(), b.transform == t).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.keys_to(super::Keys::Canvas);
                    if let Some(b) = this.ui.as_mut().and_then(|u| u.binder.as_mut()) {
                        b.transform = t;
                    }
                    cx.notify();
                }),
            ));
        }
        d = d.child(hint_line("Taken")).child(transforms);
        if b.source == Some(Source::Value) || b.transform == Transform::Equals {
            d = d.child(
                text_field("ui-bind-value-text", &b.value, editing(1), "Value")
                    .on_click(type_into(1, b.value.clone(), cx)),
            );
        }
        // What Bind will do, or why not, where the eye is.
        let Some((_, plan)) = &b.plan else {
            return Some(d);
        };
        let ready = plan.is_ok();
        // What Bind does, on a line of its own that wraps: beside the
        // buttons a long refusal pushed Cancel out of the pane.
        d = d.child(hint_line(match &plan {
            Ok(p) => format!("Binds {}", p.said),
            Err(why) => why.clone(),
        }));
        d = d.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .pt_1()
                .child(if ready {
                    gpui_editor_kit::widgets::action(
                        "Bind",
                        gpui_editor_kit::widgets::Tone::Primary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            let r = ui.bind();
                            this.ui_tree_result(r);
                        }
                        cx.notify();
                    }))
                } else {
                    gpui_editor_kit::widgets::action(
                        "Bind",
                        gpui_editor_kit::widgets::Tone::Secondary,
                    )
                    .opacity(0.5)
                })
                .child(
                    button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.binder = None;
                        }
                        cx.notify();
                    })),
                ),
        );
        Some(d)
    }
}

/// Picks where the binder's value comes from.
fn pick_source(
    src: Source,
    cx: &Context<Editor>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this: &mut Editor, _: &ClickEvent, _, cx| {
        // The search stops taking keys (round 6 typed into it after a pick).
        this.keys_to(super::Keys::Canvas);
        if let Some(b) = this.ui.as_mut().and_then(|u| u.binder.as_mut()) {
            // A fixed value is only taken as is (see `UiEditor::bind`).
            if src == Source::Value {
                b.transform = Transform::AsIs;
            }
            b.source = Some(src.clone());
        }
        cx.notify();
    })
}

/// Starts typing into the binder's search (0) or value (1).
fn type_into(
    field: u8,
    text: String,
    cx: &Context<Editor>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this: &mut Editor, _: &ClickEvent, window, cx| {
        // The field being typed keeps its edit; the press places the caret.
        let typing =
            |b: &super::bindings::Binder| b.edit.as_ref().is_some_and(|e| e.field == field);
        if this
            .ui
            .as_ref()
            .and_then(|ui| ui.binder.as_ref())
            .is_some_and(typing)
        {
            return;
        }
        this.focus.focus(window, cx);
        this.keys_to(super::Keys::Canvas);
        if let Some(b) = this.ui.as_mut().and_then(|ui| ui.binder.as_mut()) {
            b.edit = Some(TextEdit::new(field, text.clone(), Filter::Text));
        }
        cx.notify();
    })
}

fn hint_line(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(10.))
        .text_color(theme::get().muted)
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::{Catalog, Source, Target, Transform};
    use crate::ui::UiEditor;
    use crate::ui::test_support::{
        BUTTON, ICON_ROW, bindings, input, install, key, labels, node, open, path, reopen, select,
    };
    use anno_formats::phx::binding::Slot;
    use anno_formats::phx::{DefaultValue, Record};

    /// The binder opened on the selection, set as a modder picks it.
    fn pick(ui: &mut UiEditor, target: Target, transform: Transform, source: Source, value: &str) {
        ui.open_binder();
        let b = ui.binder.as_mut().unwrap();
        b.target = target;
        b.transform = transform;
        b.source = Some(source);
        b.value = value.into();
    }

    fn count(ui: &UiEditor) -> usize {
        bindings(&ui.asset).bindings.len()
    }

    // Needs the game: ColorTint, Size and RotationAngle bind from scratch,
    // as the game's files bind them everywhere (round 14: only eight
    // properties could be bound anew). A fixed tint is typed as three
    // channels or hex; each bind is one undo step and the canvas draws it.
    #[test]
    fn tint_size_and_rotation_bind_from_scratch() {
        use anno_formats::phx::binding::{NewBinding, NewSlot};
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let name = "TEST_Debug_text_01";
        select(&mut ui, name);
        let before = count(&ui);
        let bind = |ui: &mut UiEditor, target, source, value: &str| {
            pick(ui, target, Transform::AsIs, source, value);
            ui.bind()
        };
        assert!(bind(&mut ui, Target::Color, Source::Value, "[255, 300, 0]").is_err());
        bind(&mut ui, Target::Color, Source::Value, "ff0000").unwrap();
        // Stored as the game's files keep a fixed tint: a ColorType object
        // with its own colour, not a bare Vector3 (round 15).
        let tint_span = |ui: &UiEditor| {
            let t = bindings(&ui.asset);
            t.constants[&t.bindings.last().unwrap().sources[0]]
        };
        let span = tint_span(&ui);
        assert!(matches!(
            ui.asset.spans()[span].record,
            Record::Default(DefaultValue::Object(_))
        ));
        assert!(crate::ui::constant_colour(&ui.asset, span).is_some());
        // A later edit is checked as a colour: refused whole, nothing
        // changed; hex takes.
        let bytes = ui.asset.encode();
        assert!(ui.commit(span, "[255, 300, -1]").is_err());
        assert_eq!(ui.asset.encode(), bytes);
        ui.commit(span, "00ff00").unwrap();
        assert_eq!(ui.frames[node(&ui, name)].bound.tint, Some([0, 255, 0]));
        // Saved and opened again, still the tint it was given.
        let reopened = reopen(&ui.asset);
        let rgb = crate::ui::constant_colour(&reopened, span).map(|c| &reopened.spans()[c].record);
        assert!(matches!(rgb, Some(Record::Color([0, 255, 0]))), "{rgb:?}");
        assert!(ui.undo(false));
        assert_eq!(ui.frames[node(&ui, name)].bound.tint, Some([255, 0, 0]));
        bind(&mut ui, Target::Rotation, Source::Value, "12").unwrap();
        let size = input(&ui, "-Icon_Size");
        bind(&mut ui, Target::Width, Source::Input(size), "").unwrap();
        assert_eq!(count(&ui), before + 3);
        let bound = ui.frames[node(&ui, name)].bound;
        assert_eq!(bound.tint, Some([255, 0, 0]));
        assert_eq!(bound.rotation, Some(12.));
        assert_eq!(bound.size.map(|s| s[0]), Some(24.), "{:?}", bound.size);
        assert!(ui.undo(false));
        assert_eq!(count(&ui), before + 2, "one step each");
        // A three-number Color, as an earlier build wrote one, is checked
        // as a colour too.
        let w = ui.nodes[node(&ui, name)].widget;
        ui.asset = ui
            .asset
            .add_binding(
                &ui.asset.widgets(),
                &NewBinding {
                    converter: super::ASSIGN,
                    sources: vec![NewSlot::Constant {
                        kind: super::T_COLOR,
                        value: DefaultValue::Vec([255f32, 0., 0.].map(f32::to_bits).to_vec()),
                    }],
                    targets: vec![NewSlot::Widget {
                        widget: w,
                        path: Target::Color.path(),
                    }],
                },
            )
            .unwrap();
        let span = tint_span(&ui);
        assert!(ui.commit(span, "[255, 300, -1]").is_err());
        ui.commit(span, "0000ff").unwrap();
    }

    // Needs the game: the plan under Bind follows the binder's choices, and
    // is worked out when they change, not on every render. The binder
    // refuses what the graph refuses: a yes/no template input into Opacity
    // adds nothing. "Visible equals <enum>" binds, the value typed as the
    // enum's entry id the way the shipped files compare one (round 7: the
    // binder's own kind table refused what the graph and the game take);
    // As is goes by the graph's rule: an enum into Opacity stays refused.
    #[test]
    fn the_plan_follows_the_choices_and_refuses_the_wrong_kind() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        select(&mut ui, "Icon");
        let before = count(&ui);
        ui.open_binder();
        // It opens on what modders bind first.
        assert_eq!(ui.binder.as_ref().unwrap().target, Target::Visible);
        let plan = |ui: &mut UiEditor| {
            ui.refresh_bind_plan();
            match &ui.binder.as_ref().unwrap().plan {
                Some((_, Ok(p))) => p.said.clone(),
                Some((_, Err(why))) => format!("no: {why}"),
                None => "none".into(),
            }
        };
        assert_eq!(plan(&mut ui), "no: Pick where the value comes from first");
        let yes_no = input(&ui, "-Bg_IsVisible");
        ui.binder.as_mut().unwrap().source = Some(Source::Input(yes_no));
        assert_eq!(plan(&mut ui), "Visible of Icon");
        ui.binder.as_mut().unwrap().target = Target::Alpha;
        let said = plan(&mut ui);
        assert!(said.contains("Boolean"), "{said}");
        // Named as the graph and Data name it.
        assert!(said.contains("AlphaTint"), "{said}");
        let err = ui.bind().unwrap_err();
        assert!(format!("{err:#}").contains("Boolean"), "{err:#}");
        assert_eq!(count(&ui), before);

        let alignment = input(&ui, "-Alignment_IconPosition");
        assert_eq!(ui.asset.properties()[alignment].kind, super::T_ENUM);
        let source = Source::Input(alignment);
        pick(
            &mut ui,
            Target::Visible,
            Transform::Equals,
            source,
            "1569692723",
        );
        let said = ui.bind().unwrap();
        assert!(said.starts_with("Bound Visible of Icon"), "{said}");
        let t = bindings(&ui.asset);
        let added = t.bindings.last().unwrap();
        assert_eq!(
            t.slots[added.sources[1]],
            Slot::Constant {
                kind: Some(super::T_ENUM),
                value: DefaultValue::U32(1569692723)
            }
        );
        assert!(ui.undo(false));
        // As is into Opacity: an enum is no number.
        pick(
            &mut ui,
            Target::Alpha,
            Transform::AsIs,
            Source::Input(alignment),
            "",
        );
        let why = ui.bind_plan().err().unwrap();
        assert!(
            why.contains("takes Float32, and that source is Enum"),
            "{why}"
        );
    }

    // Needs the game: what the binder writes is sound. A fixed value is
    // taken as is (round 4: "equals true" compared true with "true" and hid
    // the minimap); NaN and infinities are refused; binding a property a
    // copy already sets replaces that copy in one step instead of stacking
    // a second writer, and says so. It refuses a loop as the graph does:
    // once the Icon's Visible feeds the IsScalable_Content input, binding
    // Visible from that input would read itself.
    #[test]
    fn the_binder_writes_one_sound_binding_per_property() {
        use crate::ui::graph::wiring::Drop;
        use anno_formats::phx::binding::builtin;
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let node = select(&mut ui, "Icon");
        let widget = ui.nodes[node].widget;
        let before = count(&ui);
        let fixed = |ui: &mut UiEditor, target, transform, value: &str| {
            pick(ui, target, transform, Source::Value, value);
            ui.bind()
        };
        assert!(fixed(&mut ui, Target::Visible, Transform::Equals, "true").is_err());
        for bad in ["NaN", "inf", "-inf"] {
            assert!(
                fixed(&mut ui, Target::Alpha, Transform::AsIs, bad).is_err(),
                "{bad}"
            );
        }
        assert_eq!(count(&ui), before, "refusals change nothing");
        // Visible ← -Bg_IsVisible, then Visible ← -IsScalable_Content.
        let visible_from = |ui: &mut UiEditor, name: &str| {
            let source = Source::Input(input(ui, name));
            pick(ui, Target::Visible, Transform::AsIs, source, "");
        };
        visible_from(&mut ui, "-Bg_IsVisible");
        ui.bind().unwrap();
        assert_eq!(count(&ui), before + 1);
        visible_from(&mut ui, "-IsScalable_Content");
        let said = ui.bind_plan().map(|p| p.said).unwrap();
        assert!(
            said.contains("replacing Bg_IsVisible"),
            "said before: {said}"
        );
        let said = ui.bind().unwrap();
        assert_eq!(count(&ui), before + 1, "replaced, not stacked");
        assert!(said.contains("replacing Bg_IsVisible"), "{said}");
        let follows = ui.follows(node);
        let visible: Vec<_> = follows.iter().filter(|f| f.0 == "Visible").collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1, "follows IsScalable_Content");
        // One undo step brings the first back.
        assert!(ui.undo(false));
        assert_eq!(count(&ui), before + 1);
        let visible = ui.follows(node).into_iter().find(|f| f.0 == "Visible");
        assert_eq!(
            visible.map(|f| f.1).as_deref(),
            Some("follows Bg_IsVisible")
        );

        // The loop: Visible (bound to -Bg_IsVisible) wired into the input.
        let t = bindings(&ui.asset);
        let visible = (0..t.slots.len())
            .find(|&s| {
                matches!(&t.slots[s], Slot::Widget { widget: Some(w), path }
                    if *w == widget && path[..] == [(1, 0x8000_0000 | builtin::VISIBLE)])
            })
            .unwrap();
        let scalable = input(&ui, "-IsScalable_Content");
        let to = (0..t.slots.len())
            .find(|&s| t.slots[s] == Slot::Template(scalable, 1))
            .unwrap();
        ui.select_widget(node);
        ui.wire_up(visible, None, Drop::Slot(to)).unwrap();
        visible_from(&mut ui, "-IsScalable_Content");
        let why = ui.bind_plan().err().expect("a loop");
        assert!(why.contains("would read itself"), "{why}");
    }

    // Needs the game: an axis and its whole vector are one place. XIcon's
    // Position is set whole by MOBO_01: binding X there is refused before
    // the click, saying why (round 5 stacked Position.x on it and the box
    // jumped), and Bind changes nothing.
    #[test]
    fn the_binder_says_what_bind_does_before_the_click() {
        let Some(mut ui) = open("027cffcb-adec-48e2-a715-d815a12e32ea") else {
            return;
        };
        select(&mut ui, "XIcon");
        let size = input(&ui, "-Size");
        ui.open_binder();
        ui.binder.as_mut().unwrap().target = Target::X;
        assert_eq!(
            ui.bind_plan().err().as_deref(),
            Some("Pick where the value comes from first")
        );
        ui.binder.as_mut().unwrap().source = Some(Source::Input(size));
        let why = ui.bind_plan().err().expect("refused before the click");
        assert!(why.contains("Position is set whole by MOBO_01"), "{why}");
        let bytes = ui.asset.encode();
        assert!(ui.bind().is_err());
        assert_eq!(ui.asset.encode(), bytes);
    }

    // Needs the game: binding the quest tracker container's Visible to a
    // yes/no field of the screen's game data, inverted, adds one binding;
    // switching that game data then shows and hides the container, and
    // undo takes the binding out again.
    #[test]
    fn binding_game_data_to_visible_follows_the_data() {
        let Some(install) = install() else {
            return;
        };
        let catalog = Catalog::of(&install);
        let mut ui = UiEditor::load(install, &path("minimapscene")).unwrap();
        // A yes/no field the minimap's data hands to its content, found
        // through the hand-off into the content template.
        let (prop, root, fields) = ui
            .data_roots(&catalog)
            .into_iter()
            .find_map(|(prop, name, paths)| {
                let p = paths.into_iter().find(|p| {
                    p.len() >= 2
                        && p.last()
                            .and_then(|&f| anno_formats::phx::data_field_type(f))
                            == Some(0)
                        && p.iter()
                            .all(|&f| anno_formats::phx::data_field_name(f).is_some())
                })?;
                Some((prop, name, p))
            })
            .expect("a named yes/no field of the screen's game data");
        let node = (1..ui.nodes.len())
            .find(|&n| !ui.nodes[n].foreign && ui.nodes[n].visible)
            .unwrap();
        let name = ui.nodes[node].name.clone();
        ui.select_widget(node);
        let before = count(&ui);
        let source = Source::Data {
            prop,
            fields: fields.clone(),
        };
        pick(&mut ui, Target::Visible, Transform::Invert, source, "");
        ui.bind().unwrap();
        assert_eq!(count(&ui), before + 1);
        let node = (1..ui.nodes.len())
            .find(|&n| !ui.nodes[n].foreign && ui.nodes[n].name == name)
            .unwrap();
        let views = ui.binding_views(node).unwrap().views;
        assert!(
            views
                .iter()
                .any(|v| v.writes && v.raw == "LogicalInvert" && v.converter == "not")
        );
        // The sampler answers the field under its path of names.
        let names = fields
            .iter()
            .map(|&f| anno_formats::phx::data_field_name(f).unwrap());
        let path = std::iter::once(root.as_str())
            .chain(names)
            .collect::<Vec<_>>()
            .join(".");
        let shown = |ui: &UiEditor| {
            ui.nodes
                .iter()
                .any(|n| !n.foreign && n.name == name && n.visible)
        };
        for on in [true, false] {
            crate::ui::preview::set_game_flag(&path, Some(on));
            ui.relayout();
            assert_eq!(shown(&ui), !on, "{path} = {on}");
        }
        crate::ui::preview::set_game_flag(&path, None);
        assert!(ui.undo(false));
        assert_eq!(count(&ui), before);
    }

    // A logic node's lone input has a name: what it takes, else "in"; copy
    // and not stay bare, their value is the whole story (round 13).
    #[test]
    fn a_lone_input_is_named() {
        let name = |c: &str| super::logic_name(c, 1).1[0].clone();
        assert_eq!(name("FalseToTrueChange"), "value");
        assert_eq!(name("EventToTrue"), "event");
        assert_eq!(name("EnumToBoolean"), "enum");
        assert_eq!(name("Floor"), "value");
        assert_eq!(name("SomethingNew"), "in");
        assert_eq!(name("Assign"), "");
        assert_eq!(name("TwoWayAssign"), "");
        assert_eq!(name("LogicalInvert"), "");
        assert_eq!(super::logic_name("SomethingNew", 2).1, ["a", "b"]);
    }

    // Needs the game: a slot inside a nested template names each step, the
    // last a clip there when it is one (round 15: the quest tracker's
    // "BtnOverlay_DS › · Start" said nothing of which clip it starts). The
    // status after removing a binding names its places as the graph does
    // (review R16O5: unbinding OnMouseLeave → the nested clip Enter's Stop
    // said "BtnOverlay_DS › · Stop" and an id for the button's event).
    #[test]
    fn nested_slots_name_their_template_steps_and_clip() {
        let Some(mut ui) = open(BUTTON) else {
            return;
        };
        let ft = ui.file_table(0).unwrap();
        let all = labels(&ft);
        let nested: Vec<&String> = (0..all.len())
            .filter(|&s| matches!(ft.table.slots[s], Slot::Nested { .. }))
            .map(|s| &all[s])
            .collect();
        assert!(!nested.is_empty());
        assert!(nested.iter().all(|l| !l.contains("› ·")), "{nested:?}");
        assert!(
            nested.iter().any(|l| l.starts_with("BtnOverlay_DS › ")
                && l.contains("clip ")
                && l.ends_with("Start")),
            "{nested:?}"
        );
        let stop = all
            .iter()
            .position(|l| l == "BtnOverlay_DS › clip Enter · Stop")
            .expect("the nested clip's Stop");
        let binding = ft
            .table
            .bindings
            .iter()
            .position(|b| b.targets.contains(&stop))
            .expect("a binding stops it");
        let said = ui.unbind(&[binding]).unwrap();
        let hex = said
            .as_bytes()
            .windows(8)
            .any(|w| w.iter().all(u8::is_ascii_hexdigit));
        assert!(
            said.contains("clip Enter") && said.contains("OnMouseLeave"),
            "{said}"
        );
        assert!(!said.contains("› ·") && !hex, "{said}");
    }

    // Needs the game: the quest tracker's jump button reads its nested
    // button's state by names, not ids. Round 3 showed
    // "198eaf32.20e8e1df ← AnnoButtonWT · 70da9535.75a09f6b".
    #[test]
    fn nested_button_paths_have_names() {
        let Some(mut ui) = open("questtrackerscene") else {
            return;
        };
        let node = (1..ui.nodes.len())
            .find(|&n| &*ui.nodes[n].name == "BtnOverlay_DS")
            .unwrap();
        let labels = labels(&ui.file_table(node).unwrap());
        let has = |l: &str| labels.iter().any(|x| x == l);
        // A component on the container's template root.
        assert!(has("AnnoButtonWT · Focus.IsFocused"), "{labels:#?}");
        // Native properties up the root's WidgetType chain.
        assert!(has("AnnoButtonWT · OnMouseEnter"));
        assert!(has("AnnoButtonWT · OnMouseLeave"));
        // A field of an object input keeps the input's name.
        assert!(has("BtnOverlay_DS · -BtnEvents.Leave"), "{labels:#?}");
        assert!(
            !labels
                .iter()
                .any(|l| l.contains("70da9535") || l.contains("198eaf32"))
        );
    }

    // Needs the game: no hash where a shipped table has a name. The icon
    // row's Text container reads its Layout fields by name (round 6 showed
    // `Layout.20517da9`); the minimap's tracks name the template input they
    // animate (round 6 showed `131547d4`).
    #[test]
    fn fields_and_tracks_read_by_name() {
        let Some(install) = install() else {
            return;
        };
        let mut ui = UiEditor::load(install.clone(), &path(ICON_ROW)).unwrap();
        let text = node(&ui, "Text");
        let labels = labels(&ui.file_table(text).unwrap());
        assert!(
            labels.iter().any(|l| l.contains("Layout.Overflow_Y")),
            "{labels:#?}"
        );
        let ui = UiEditor::load(install, &path("minimapscene")).unwrap();
        let tracks: Vec<&str> = ui
            .player
            .clips
            .iter()
            .flat_map(|c| c.lanes.iter().map(|l| l.label.as_str()))
            .collect();
        assert!(
            !tracks.iter().any(|t| t.contains("131547d4")),
            "{tracks:#?}"
        );
        // The focus controller's IsFocused clip animates its dispatcher's
        // input by name.
        let focused = ui
            .player
            .clips
            .iter()
            .find(|c| c.owner == "Focus_HUD_Controller" && c.name == "IsFocused")
            .expect("the IsFocused clip");
        let lanes: Vec<_> = focused.lanes.iter().map(|l| &l.label).collect();
        assert!(
            lanes
                .iter()
                .any(|l| *l == "AnimationDispatcher_DS · Value_01"),
            "{lanes:?}"
        );
    }

    // Needs the game: a fixed colour in the List knows its colour (a swatch
    // that opens the picker when it is editable), as the graph's pin does.
    #[test]
    fn list_chips_know_their_colour() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let node = select(&mut ui, "Icon");
        let views = ui.binding_views(node).unwrap().views;
        let chip = views
            .iter()
            .flat_map(|v| &v.sources)
            .find(|c| c.rgb.is_some())
            .expect("a colour chip");
        assert_eq!(chip.rgb, Some([255, 255, 255]));
        assert!(
            chip.constant.is_some(),
            "editable: the swatch opens the picker"
        );
        // Esc closes an open picker with the canvas's keys too.
        let span = chip.constant.unwrap();
        ui.open_pick_for_test(span, [255, 255, 255]);
        assert!(ui.key_down(&key("escape")));
        assert!(!ui.picking(), "closed");
        // And with the Bindings pane's keys, the List shown.
        ui.take_keys(crate::ui::Keys::Graph);
        ui.open_pick_for_test(span, [255, 255, 255]);
        assert!(ui.graph_key_down(&key("escape")));
        assert!(!ui.picking(), "closed over the List");
    }

    // Needs the game: the HUD icon-text template sizes its icon as a
    // constant 16 × the interface scale. The pane lists that binding with
    // its constant editable; changing it re-evaluates with the new value.
    #[test]
    fn constants_edit_in_place_and_reevaluate() {
        let Some(mut ui) = open("ae3fe300-1235-46e6-a9c0-c9e68720b44a") else {
            return;
        };
        ui.bindings_all = true;
        let super::FileBindings { file, views, .. } = ui.binding_views(0).unwrap();
        assert!(file.is_none());
        let (i, span) = views
            .iter()
            .enumerate()
            .find_map(|(i, v)| {
                (v.raw == "Multiply")
                    .then(|| v.sources.iter().find_map(|c| c.constant).map(|s| (i, s)))
                    .flatten()
            })
            .expect("the icon size binding with its constant");
        assert!(views[i].result.ends_with("→ 16"), "{}", views[i].result);
        ui.commit(span, "20").unwrap();
        let views = ui.binding_views(0).unwrap().views;
        assert!(views[i].result.ends_with("→ 20"), "{}", views[i].result);
    }
}
