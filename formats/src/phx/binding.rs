// Data bindings of a widget template: the binding tables decoded, and the
// edits that add, copy, rewire and remove bindings. The shell's UI editor
// evaluates them (src/ui/evaluate.rs).
//
// The root holds the binding tables. A binding (`0xedcf0433`) copies from
// its source slots to its target slots; slot indices run through one table,
// PropertyContainers first, then the template-property containers, then the
// constants (checked on 164,931 indices in the install; no target is ever a
// constant). A PropertyContainer names a property of a widget: a provider
// (a path to a widget, `0x8fda299c`) and a PropertyPath, either one
// built-in widget property id (`0x80000001` Position …) or a component
// instance UniqueId and one of its custom property UniqueIds.

use std::collections::HashMap;

use super::{
    Asset, Class, DefaultValue, Field, Guid, ObjectId, Prop, Record, Value, Widget, name_crc,
};

/// Where a binding reads or writes.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    /// A widget's property: the widget (index into [`Asset::widgets`]; None
    /// when the provider path does not resolve in this file) and the path of
    /// `(PropertyField, UniqueId)` pairs.
    Widget {
        widget: Option<usize>,
        path: Vec<(u32, u32)>,
    },
    /// A widget inside a container's template: the container (index into
    /// [`Asset::widgets`]), then the object ids of the widgets the provider
    /// steps through below it (DataMode 5 then 0: a template writing a
    /// nested template's label), and the property path.
    Nested {
        widget: usize,
        inner: Vec<Guid>,
        path: Vec<(u32, u32)>,
    },
    /// A property of one of this file's clips (index into
    /// [`Asset::clips`]): Start, Stop, Completed (`0x8000001f`…).
    Clip { clip: usize, path: Vec<(u32, u32)> },
    /// The template's own custom property, by [`Asset::properties`] index,
    /// and the PropertyField (1 whole value, 2/3 one axis of a vector).
    Template(usize, u32),
    /// A fixed value and the PropertyType the file stores with it (three
    /// floats can be a Vector3 or a colour; the value alone does not say).
    Constant {
        kind: Option<u32>,
        value: DefaultValue,
    },
}

impl Slot {
    /// A PropertyId UniqueId with this bit set names a built-in widget
    /// property (Position, Visible, a clip's Start…) by the id below it;
    /// without it, a template or component custom property.
    pub const BUILTIN: u32 = 0x8000_0000;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub mode: u32,
    /// Class of the binding's `0x6fd1ffc4` object: the converter that turns
    /// the sources into the value written (see [`Converter`]).
    pub converter: u32,
    pub sources: Vec<usize>,
    pub targets: Vec<usize>,
    /// EnumToBoolean rules (`BindingInfos`): (enum entry UniqueId, target
    /// index). Target k is true when the source is one of its entries.
    pub rules: Vec<(u32, usize)>,
    /// `EnumEntry`: the enum widget style this binding is live in; 0 in all.
    pub entry: u32,
    /// EnumToValue: the EnumAsset (`PHXEN`) and the RuleId of its
    /// ConversionRule that maps entries to values.
    pub enum_rule: Option<(Guid, u32)>,
}

/// Binding converters. Class crcs are `crc32("<Name>ConverterInstance")`
/// (see `Converter::BY_CLASS`); designers see a looser name on the template property
/// that holds the result (`Addition`, `IfTrueFalseAssignment`,
/// `SignConverter`…). Sign is the one class with no name found. Operand order
/// read from shipped bindings (Clamp `value, min, max`, Divide `a / b`,
/// IfTrueFalseAssign `if true, if false, condition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Converter {
    Assign,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Min,
    Max,
    Clamp,
    Floor,
    Round,
    Abs,
    /// The `SignConverter`: flips the sign (shipped use: −Size/2 offsets).
    // TODO(alex): whether it flips or returns −1/0/1 is inferred from usage.
    Sign,
    Sin,
    Cos,
    DegreeToRadian,
    SquareRoot,
    Equal,
    NotEqual,
    /// `LesserConverterInstance`: first < second.
    LessThan,
    /// `LesserOrEqualConverterInstance`.
    LessOrEqual,
    IsInRangeButNotEqual,
    Greater,
    GreaterOrEqual,
    IsInRange,
    LogicalAnd,
    LogicalOr,
    /// `LogicalXOrConverterInstance`.
    LogicalXor,
    LogicalInvert,
    /// `IsEmpty` (`0x25838498`, new with the 2026-11-05 patch): whether a
    /// string or value is empty (the savegame popup hides empty lists).
    IsEmpty,
    IfTrueAssign,
    IfTrueFalseAssign,
    IfFalseAssign,
    ConcatValues,
    ValueToString,
    NumberToFloat,
    NumberToBool,
    BoolToNumber,
    Ceil,
    /// Per target: whether the source enum is one of the target's entries
    /// (`Binding::rules`).
    EnumToBoolean,
    /// The value an EnumAsset's ConversionRule gives the source entry
    /// (`Binding::enum_rule`; text styles → font sizes).
    EnumToValue,
    /// `EventAssign` (`0xa3de9f56`, 334 uses): value, event → target; the
    /// value lands when the event fires. Animation dispatchers tween their
    /// output to it; the editor shows the screen settled, as if it fired.
    // TODO(alex): the event is not evaluated; every EventAssign counts as
    // fired.
    EventAssign,
    /// `EventToTrue` / `EventToFalse` (`0xc32c7054` / `0x130e634e`): set
    /// the target when the event fires. Evaluated only for the root's
    /// built-in event Shown (`0x3c`); hover, focus and value
    /// changes are input the static view has not had.
    EventToTrue,
    EventToFalse,
}

impl Converter {
    /// The PropertyTypes argument `k` takes, `None` for any: the sets the
    /// shipped files feed each converter (the editor's golden check holds
    /// every shipped argument to them). Yes/no logic and if/else
    /// conditions take yes/no only, enum converters an enum (or its text),
    /// events an event; arithmetic takes numbers, vectors and colours (and
    /// yes/no as 0/1), never text, an enum's entry id or an event; ordered
    /// comparisons numbers or text. Values passed through (if/else values,
    /// Equal, Concat, ToString) take anything.
    pub fn takes(self, k: usize) -> Option<&'static [u32]> {
        const BOOL: &[u32] = &[0x00];
        const EVENT: &[u32] = &[0x09];
        const ENUM: &[u32] = &[0x11, 0x04];
        // Yes/no as 0/1 only where the files subtract one (5 uses); a
        // yes/no into a multiplication is a mistake (round 5 wired one).
        const NUMERIC: &[u32] = &[0x01, 0x02, 0x03, 0x05, 0x06, 0x0d, 0x0e, 0x12];
        const NUMERIC_OR_BOOL: &[u32] = &[0x00, 0x01, 0x02, 0x03, 0x05, 0x06, 0x0d, 0x0e, 0x12];
        const ORDERED: &[u32] = &[0x01, 0x02, 0x03, 0x04];
        use Converter::*;
        Some(match self {
            LogicalAnd | LogicalOr | LogicalXor | LogicalInvert | BoolToNumber => BOOL,
            IfTrueAssign | IfFalseAssign if k == 1 => BOOL,
            IfTrueFalseAssign if k == 2 => BOOL,
            EnumToBoolean | EnumToValue => ENUM,
            EventToTrue | EventToFalse => EVENT,
            EventAssign if k >= 1 => EVENT,
            Subtract if k == 1 => NUMERIC_OR_BOOL,
            Add | Subtract | Multiply | Divide | Modulo | Min | Max | Clamp | Floor | Round
            | Abs | Sign | Sin | Cos | DegreeToRadian | SquareRoot | Ceil | NumberToFloat
            | NumberToBool => NUMERIC,
            LessThan | LessOrEqual | Greater | GreaterOrEqual | IsInRange
            | IsInRangeButNotEqual => ORDERED,
            _ => return None,
        })
    }

    /// Whether argument `k` is written to the target as it is (a copy, the
    /// value an if / else passes on): it takes what the target takes.
    pub fn passes(self, k: usize) -> bool {
        use Converter::*;
        matches!(
            (self, k),
            (Assign | IfTrueAssign | IfFalseAssign, 0) | (IfTrueFalseAssign, 0 | 1)
        )
    }
}

/// Every converter class name, for display: the evaluated ones and the
/// event converters the editor does not run (an event fires on input or a
/// value change, not in a static view).
pub fn converter_name(class: u32) -> Option<&'static str> {
    const NAMES: [(u32, &str); 9] = [
        (name_crc(b"TwoWayAssignConverterInstance"), "TwoWayAssign"),
        (name_crc(b"EventToTrueConverterInstance"), "EventToTrue"),
        (name_crc(b"EventToFalseConverterInstance"), "EventToFalse"),
        (name_crc(b"EventAssignConverterInstance"), "EventAssign"),
        (name_crc(b"ValueChangeConverterInstance"), "ValueChange"),
        (
            name_crc(b"TrueToFalseChangeConverterInstance"),
            "TrueToFalseChange",
        ),
        (
            name_crc(b"FalseToTrueChangeConverterInstance"),
            "FalseToTrueChange",
        ),
        (name_crc(b"IsEmptyConverterInstance"), "IsEmpty"),
        // GetCount (new with the 2026-11-05 patch): an array's length.
        // Arrays come from game data, which the editor does not have.
        (name_crc(b"GetCountConverterInstance"), "GetCount"),
    ];
    NAMES.iter().find(|n| n.0 == class).map(|n| n.1)
}

impl Converter {
    /// Converter classes the editor evaluates.
    const BY_CLASS: [(u32, Converter); 46] = {
        use Converter::*;
        [
            (name_crc(b"AssignConverterInstance"), Assign),
            // TwoWayAssign: the converter of every two-way binding
            // (BindingMode 1, 275 uses: text input text, selected index,
            // scroll info). Forward it is a plain copy.
            (name_crc(b"TwoWayAssignConverterInstance"), Assign),
            (name_crc(b"EventAssignConverterInstance"), EventAssign),
            (name_crc(b"EventToTrueConverterInstance"), EventToTrue),
            (name_crc(b"EventToFalseConverterInstance"), EventToFalse),
            (name_crc(b"AddConverterInstance"), Add),
            (name_crc(b"SubtractConverterInstance"), Subtract),
            // Older files spell it Substract.
            (name_crc(b"SubstractConverterInstance"), Subtract),
            (name_crc(b"MultiplyConverterInstance"), Multiply),
            (name_crc(b"DivideConverterInstance"), Divide),
            (name_crc(b"ModuloConverterInstance"), Modulo),
            (name_crc(b"MinConverterInstance"), Min),
            (name_crc(b"MaxConverterInstance"), Max),
            (name_crc(b"ClampConverterInstance"), Clamp),
            (name_crc(b"FloorConverterInstance"), Floor),
            (name_crc(b"RoundConverterInstance"), Round),
            (name_crc(b"AbsConverterInstance"), Abs),
            // Name not found (it is not SignConverterInstance).
            (0x8bd8_5eca, Sign),
            (name_crc(b"SinConverterInstance"), Sin),
            (name_crc(b"CosConverterInstance"), Cos),
            (name_crc(b"DegreeToRadianConverterInstance"), DegreeToRadian),
            (name_crc(b"SquareRootConverterInstance"), SquareRoot),
            (name_crc(b"EqualConverterInstance"), Equal),
            (name_crc(b"NotEqualConverterInstance"), NotEqual),
            (name_crc(b"LesserConverterInstance"), LessThan),
            (name_crc(b"LesserOrEqualConverterInstance"), LessOrEqual),
            (
                name_crc(b"IsInRangeButNotEqualConverterInstance"),
                IsInRangeButNotEqual,
            ),
            (name_crc(b"GreaterConverterInstance"), Greater),
            (name_crc(b"GreaterOrEqualConverterInstance"), GreaterOrEqual),
            (name_crc(b"IsInRangeConverterInstance"), IsInRange),
            (name_crc(b"LogicalAndConverterInstance"), LogicalAnd),
            (name_crc(b"LogicalOrConverterInstance"), LogicalOr),
            (name_crc(b"LogicalInvertConverterInstance"), LogicalInvert),
            (name_crc(b"IsEmptyConverterInstance"), IsEmpty),
            (name_crc(b"IfTrueAssignConverterInstance"), IfTrueAssign),
            (name_crc(b"ValueToStringConverterInstance"), ValueToString),
            (name_crc(b"NumberToBoolConverterInstance"), NumberToBool),
            (name_crc(b"CeilConverterInstance"), Ceil),
            (name_crc(b"EnumToBooleanConverterInstance"), EnumToBoolean),
            (name_crc(b"EnumToValueConverterInstance"), EnumToValue),
            (name_crc(b"LogicalXOrConverterInstance"), LogicalXor),
            (
                name_crc(b"IfTrueFalseAssignConverterInstance"),
                IfTrueFalseAssign,
            ),
            (name_crc(b"IfFalseAssignConverterInstance"), IfFalseAssign),
            (name_crc(b"ConcatValuesConverterInstance"), ConcatValues),
            (name_crc(b"NumberToFloatConverterInstance"), NumberToFloat),
            (name_crc(b"BoolToNumberConverterInstance"), BoolToNumber),
        ]
    };

    pub fn of(class: u32) -> Option<Converter> {
        Converter::BY_CLASS
            .iter()
            .find(|c| c.0 == class)
            .map(|c| c.1)
    }
}

/// A file's binding tables, resolved.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bindings {
    pub slots: Vec<Slot>,
    pub bindings: Vec<Binding>,
    /// Constant slots by index → the span of their stored default, which
    /// [`super::Edit::Default`] rewrites.
    pub constants: HashMap<usize, usize>,
}

impl Asset {
    /// The variant value stored at span `span` (a CustomProperty default, a
    /// constant, a PropertyValue), with a ColorType object resolved to its
    /// RGB as three floats 0..=255, the shape colour keys and ColorTint
    /// writes use.
    pub fn default_at(&self, span: usize) -> DefaultValue {
        let Record::Default(v) = &self.spans[span].record else {
            return DefaultValue::None;
        };
        if let DefaultValue::Object(class) = v
            && *class == Class::COLOR_TYPE
        {
            // The ColorType object follows: palette ref, entry, Color.
            let (mut palette, mut entry, mut rgb) = (None, None, None);
            for s in self.spans[span + 1..].iter().take(8) {
                match &s.record {
                    Record::Ref(r) if palette.is_none() => palette = Some(r.id),
                    Record::Value(Value::U32(v)) if entry.is_none() => entry = Some(*v),
                    Record::Color(c) => {
                        rgb = Some(*c);
                        break;
                    }
                    _ => {}
                }
            }
            let rgb = palette
                .zip(entry)
                .and_then(|(g, e)| self.palette_color(g, e))
                .or(rgb);
            if let Some(rgb) = rgb {
                return DefaultValue::Vec(rgb.map(|x| f32::from(x).to_bits()).to_vec());
            }
        }
        v.clone()
    }

    // Spans of the direct values of array or scalar property `crc` of
    // `object` (objects for pointer arrays), Count spans left out.
    pub(super) fn values_of(&self, object: usize, crc: u32) -> Vec<usize> {
        let Record::Object(h) = &self.spans[object].record else {
            return Vec::new();
        };
        let Some(prop) = self
            .class_props(h.class)
            .and_then(|p| p.iter().position(|p| p.name_crc == crc))
        else {
            return Vec::new();
        };
        let field = Field {
            object,
            prop: prop as u16 + 1,
        };
        (object + 1..self.subtree_end_of(object))
            .filter(|&i| {
                self.spans[i].field == Some(field)
                    && !matches!(self.spans[i].record, Record::Count(_))
            })
            .collect()
    }

    fn subtree_end_of(&self, object: usize) -> usize {
        (object + 1..self.spans.len())
            .find(|&i| self.spans[i].field.is_none_or(|f| f.object < object))
            .unwrap_or(self.spans.len())
    }

    fn u32_of(&self, object: usize, crc: u32) -> Option<u32> {
        self.values_of(object, crc)
            .into_iter()
            .find_map(|s| match self.spans[s].record {
                Record::Value(Value::U32(v) | Value::Enum { value: v, .. }) => Some(v),
                _ => None,
            })
    }

    /// An EnumAsset's ConversionRule `rule` applied to enum entry `entry`
    /// (0: the rule's first mapped entry).
    pub fn enum_rule(&self, rule: u32, entry: u32) -> Option<DefaultValue> {
        let rules = self.enum_rules.get_or_init(|| {
            let Some((root, _)) = self.objects().next() else {
                return Vec::new();
            };
            self.values_of(root, Prop::CONVERSION_RULES)
                .into_iter()
                .filter_map(|r| {
                    let values = self
                        .values_of(r, Prop::CONVERSION_VALUES)
                        .into_iter()
                        .map(|v| {
                            let value = self
                                .values_of(v, Prop::VALUE)
                                .into_iter()
                                .find(|&s| matches!(self.spans[s].record, Record::Default(_)))
                                .map(|s| self.default_at(s));
                            (self.u32_of(v, Prop::CONVERSION_ENTRY), value)
                        })
                        .collect::<Vec<_>>();
                    Some((self.u32_of(r, Prop::RULE_ID)?, values))
                })
                .collect()
        });
        // The first rule with this id, its first value for `entry` (0: the
        // first value); one that stores nothing gives nothing.
        let (_, values) = rules.iter().find(|(id, _)| *id == rule)?;
        let (_, v) = values
            .iter()
            .find(|(e, _)| entry == 0 || *e == Some(entry))?;
        v.clone()
    }

    /// Widget index by object id.
    pub fn widget_ids(&self, widgets: &[Widget]) -> HashMap<Guid, usize> {
        widgets
            .iter()
            .enumerate()
            .filter_map(|(k, w)| match &self.spans[w.object].record {
                Record::Object(h) => match h.id {
                    ObjectId::Id(g) => Some((g, k)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The root's binding tables with providers resolved to widgets.
    /// The bytes of the constant whose stored value is span `default`, to
    /// write it again elsewhere (`NewSlot::Copied`).
    pub fn constant_bytes(&self, default: usize) -> Option<Vec<u8>> {
        let (root, _) = self.objects().next()?;
        let c = self
            .values_of(root, Prop::CONSTANTS)
            .into_iter()
            .find(|&c| self.values_of(c, Prop::DEFAULT_VALUE).contains(&default))?;
        Some(self.bytes[self.subtree_bytes(c)].to_vec())
    }

    pub fn bindings(&self, widgets: &[Widget]) -> Bindings {
        let Some(root) = widgets.first().map(|w| w.object) else {
            return Bindings::default();
        };
        let widget_by_id = self.widget_ids(widgets);
        // A provider is a path of steps; one step with no widget is the
        // template root, one with a widget is that widget. Several steps
        // that all name widgets walk from a container here into its
        // template (and further down); other paths walk into data the
        // static view does not have.
        let target = |step: usize| match self.values_of(step, Prop::TARGET_WIDGET).first() {
            Some(&s) => match self.spans[s].record {
                Record::Ref(r) => Some(Some(r.id)),
                Record::Null => Some(None),
                _ => None,
            },
            None => Some(None),
        };
        // A provider may name one of the file's clips instead of a widget.
        let clip_by_id: HashMap<Guid, usize> = self
            .clip_ids()
            .into_iter()
            .enumerate()
            .filter_map(|(i, id)| Some((id?, i)))
            .collect();
        // (widget, steps into its template), or a clip of this file.
        #[derive(Clone)]
        enum Provider {
            Widget(usize, Vec<Guid>),
            Clip(usize),
        }
        let providers: Vec<Option<Provider>> = self
            .values_of(root, Prop::PROVIDERS)
            .into_iter()
            .map(|p| {
                let steps = self.values_of(p, Prop::STEPS);
                match steps[..] {
                    [step] => match target(step)? {
                        Some(id) => match (widget_by_id.get(&id), clip_by_id.get(&id)) {
                            (Some(&w), _) => Some(Provider::Widget(w, Vec::new())),
                            (None, Some(&c)) => Some(Provider::Clip(c)),
                            (None, None) => None,
                        },
                        None => Some(Provider::Widget(0, Vec::new())),
                    },
                    [first, ref rest @ ..] => {
                        let w = *widget_by_id.get(&target(first)??)?;
                        let inner = rest
                            .iter()
                            .map(|&s| target(s).flatten())
                            .collect::<Option<Vec<Guid>>>()?;
                        // Steps through a group stay in this file: the last
                        // one is the widget.
                        if let Some(&last) = inner.last().and_then(|g| widget_by_id.get(g)) {
                            return Some(Provider::Widget(last, Vec::new()));
                        }
                        Some(Provider::Widget(w, inner))
                    }
                    [] => None,
                }
            })
            .collect();
        let mut slots = Vec::new();
        for c in self.values_of(root, Prop::CONTAINERS) {
            let path = self
                .values_of(c, Prop::PROPERTY_PATH)
                .into_iter()
                .map(|id| {
                    (
                        self.u32_of(id, Prop::FIELD).unwrap_or(1),
                        self.u32_of(id, Prop::UNIQUE_ID).unwrap_or(0),
                    )
                })
                .collect();
            let provider = self
                .u32_of(c, Prop::PROVIDER)
                .and_then(|p| providers.get(p as usize).cloned().flatten());
            slots.push(match provider {
                Some(Provider::Clip(clip)) => Slot::Clip { clip, path },
                Some(Provider::Widget(widget, inner)) if !inner.is_empty() => Slot::Nested {
                    widget,
                    inner,
                    path,
                },
                Some(Provider::Widget(w, _)) => Slot::Widget {
                    widget: Some(w),
                    path,
                },
                None => Slot::Widget { widget: None, path },
            });
        }
        for c in self.values_of(root, Prop::TEMPLATE_CONTAINERS) {
            slots.push(Slot::Template(
                self.u32_of(c, Prop::TEMPLATE_INDEX).unwrap_or(u32::MAX) as usize,
                self.u32_of(c, Prop::FIELD).unwrap_or(1),
            ));
        }
        let mut constants = HashMap::new();
        for c in self.values_of(root, Prop::CONSTANTS) {
            let span = self
                .values_of(c, Prop::DEFAULT_VALUE)
                .into_iter()
                .find(|&s| matches!(self.spans[s].record, Record::Default(_)));
            if let Some(s) = span {
                constants.insert(slots.len(), s);
            }
            slots.push(Slot::Constant {
                kind: self.u32_of(c, Prop::PROPERTY_TYPE),
                value: span.map_or(DefaultValue::None, |s| self.default_at(s)),
            });
        }
        let index = |s: usize| match self.spans[s].record {
            Record::Value(Value::U32(v)) => Some(v as usize),
            _ => None,
        };
        let bindings = self
            .values_of(root, Prop::BINDINGS)
            .into_iter()
            .map(|b| {
                let conv = self.values_of(b, Prop::CONVERTER).first().copied();
                let mode = conv
                    .and_then(|m| self.u32_of(m, Prop::BINDING_MODE))
                    .unwrap_or(0);
                let converter = conv.map_or(0, |m| match &self.spans[m].record {
                    Record::Object(h) => h.class,
                    _ => 0,
                });
                let rules = conv
                    .map(|m| {
                        self.values_of(m, Prop::BINDING_INFOS)
                            .into_iter()
                            .filter_map(|i| {
                                let entry = self.u32_of(i, Prop::RULE_ENTRY)?;
                                let target = self
                                    .values_of(i, Prop::RULE_TARGET)
                                    .first()
                                    .and_then(|&t| match self.spans[t].record {
                                        Record::Value(Value::U16(v)) => Some(usize::from(v)),
                                        _ => None,
                                    })?;
                                Some((entry, target))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Binding {
                    mode,
                    rules,
                    entry: self.u32_of(b, Prop::ENUM_ENTRY).unwrap_or(0),
                    enum_rule: conv.and_then(|m| {
                        let asset =
                            self.values_of(m, Prop::ENUM_ASSET).iter().find_map(
                                |&s| match &self.spans[s].record {
                                    Record::Ref(r) => Some(r.id),
                                    _ => None,
                                },
                            )?;
                        Some((asset, self.u32_of(m, Prop::RULE_ID)?))
                    }),
                    converter,
                    sources: self
                        .values_of(b, Prop::BINDING_SOURCES)
                        .into_iter()
                        .filter_map(index)
                        .collect(),
                    targets: self
                        .values_of(b, Prop::BINDING_TARGETS)
                        .into_iter()
                        .filter_map(index)
                        .collect(),
                }
            })
            .collect();
        Bindings {
            slots,
            bindings,
            constants,
        }
    }
}

// ---------------------------------------------------------------------------
// Authoring
//
// New bindings go at the end of the root's binding array; the slots they
// need go at the end of their table's region. Slot indices run through
// PropertyContainers, template containers and constants in that order, so
// a container added to an earlier region moves every later index: those
// stored in bindings (sources, targets) and in timeline tracks are
// renumbered in the same rebuild. Record shapes, as every shipped file
// writes them:
//
//   PropertyContainer   02 00 1bb72d66 04 70  [PropertyId] provider UseCount
//   PropertyId          02 00 6e5a7f0e 03 60  PropertyField UniqueId
//   provider            02 00 b0dd1389 02 40  [step]
//   step                02 00 7e4fae85 03 60  DataMode (1 root, 0 widget) target
//   CustomPropertyCont. 02 00 9df79719 04 70  index PropertyField UseCount
//   constant            02 00 fb2cf8c8 04 70  PropertyType id16(0) DefaultValue
//   binding             02 00 edcf0433 09 7f80 converter [src] [tgt]
//                       GameObjectTemplate EnumEntry IsStatic IsDynamic ProfileID
//   converter           02 00 <class> 05 78  BindingMode IsDynamic #src #tgt

/// Where a new binding reads or writes ([`Asset::add_binding`]).
#[derive(Debug, Clone, PartialEq)]
pub enum NewSlot {
    /// A property of this file's widget `widget` (index into
    /// [`Asset::widgets`], 0 the root): a built-in (`[(1, 0x8000_0000 |
    /// id)]`), a component property, or on the root a data path (template
    /// property uid, then game-object field ids).
    Widget {
        widget: usize,
        path: Vec<(u32, u32)>,
    },
    /// The file's own template property (index into
    /// [`Asset::properties`]) and its PropertyField (1 whole, 2.. an axis).
    Template(usize, u32),
    /// A constant of PropertyType `kind`.
    Constant { kind: u32, value: DefaultValue },
    /// A constant written as these bytes: a shipped constant's subtree
    /// (`Asset::constant_bytes`). A fixed colour is a ColorType object,
    /// which `Constant` cannot spell.
    Copied(Vec<u8>),
    /// A property of one of this file's clips (index into
    /// [`Asset::clips`]): Start, Stop … (`[(1, 0x8000_001f)]`), through a
    /// provider whose one step names the clip (DataMode 2).
    Clip { clip: usize, path: Vec<(u32, u32)> },
    /// A property of a clip (its object id) of the template container
    /// widget `widget` shows: a provider stepping to the container
    /// (DataMode 0), then to the clip (DataMode 2), as the game's buttons
    /// start their overlay's Enter.
    NestedClip {
        widget: usize,
        clip: Guid,
        path: Vec<(u32, u32)>,
    },
    /// Slot `s` of the file as it is (`Bindings::slots` index): its
    /// container used once more, as a copied binding shares one. A clip's
    /// Start or Completed and a property inside a nested template are
    /// reached through providers of their own (a clip step has DataMode 2,
    /// a nested one walks from the container into its template); sharing
    /// the container the file already has writes them as the file does.
    Existing(usize),
}

/// An existing binding to copy (see [`Asset::copy_bindings`]): its index,
/// and the slots to change, by position among its sources then targets.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingCopy {
    pub binding: usize,
    pub slots: Vec<(usize, NewSlot)>,
}

/// A binding to add: the converter class (crc of `<Name>ConverterInstance`,
/// see [`Converter::of`]) and its slots.
#[derive(Debug, Clone, PartialEq)]
pub struct NewBinding {
    pub converter: u32,
    pub sources: Vec<NewSlot>,
    pub targets: Vec<NewSlot>,
}

fn u32s(out: &mut Vec<u8>, values: &[u32]) {
    crate::varint::write(out, values.len() as u32);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Where a planned slot sits: in the PropertyContainers, the template
/// containers or the constants, by index within its region.
pub(super) enum At {
    P(usize),
    T(usize),
    C(usize),
}

/// Inserts `items` at the end of `array` (count span, elements).
pub(super) fn append(
    asset: &Asset,
    splices: &mut Vec<super::Splice>,
    counts: &mut HashMap<usize, i64>,
    array: &(usize, Vec<usize>),
    items: Vec<Vec<u8>>,
) {
    if items.is_empty() {
        return;
    }
    let at = asset.array_slot(array.0, &array.1, usize::MAX);
    *counts.entry(array.0).or_default() += items.len() as i64;
    splices.push(super::Splice {
        range: at..at,
        bytes: items.concat(),
    });
}

/// One provider step: its DataMode (0 a widget, 1 the root, 2 a clip) and
/// the object it names (None: the root).
type Step = (u32, Option<Guid>);

/// The slots one edit of a file needs (a binding's, a timeline track's):
/// existing ones found, new containers, providers and constants planned,
/// and on `finish` the splices that add them and move every stored slot
/// index past them (bindings' sources and targets, tracks' targets).
/// The root's five binding tables, each its count span and elements: the
/// slot tables in slot order, then the providers and the bindings.
pub(super) struct Tables {
    containers: (usize, Vec<usize>),
    templates: (usize, Vec<usize>),
    constants: (usize, Vec<usize>),
    providers: (usize, Vec<usize>),
    pub(super) bindings: (usize, Vec<usize>),
}

pub(super) struct SlotPlan<'a> {
    asset: &'a Asset,
    widgets: &'a [Widget],
    table: Bindings,
    pub(super) tables: Tables,
    /// Each provider's steps: (DataMode, target; None the root), None when
    /// a step names something else.
    steps: Vec<Option<Vec<Step>>>,
    new_p: Vec<Vec<u8>>,
    new_t: Vec<Vec<u8>>,
    new_c: Vec<Vec<u8>>,
    new_providers: Vec<Vec<Step>>,
    /// Uses added to existing containers, by container object.
    uses: HashMap<usize, u32>,
}

impl<'a> SlotPlan<'a> {
    /// The classes a [`SlotPlan`] may add.
    pub(super) const CLASSES: [u32; 6] = [
        Class::PROPERTY_CONTAINER,
        Class::PROPERTY_ID,
        Class::PROVIDER,
        Class::STEP,
        Class::TEMPLATE_CONTAINER,
        Class::CONSTANT,
    ];

    pub(super) fn new(asset: &'a Asset, widgets: &'a [Widget]) -> anyhow::Result<SlotPlan<'a>> {
        let root = widgets
            .first()
            .map(|w| w.object)
            .ok_or_else(|| anyhow::anyhow!("no widgets"))?;
        let tables = asset.binding_tables(root)?;
        let steps = tables
            .providers
            .1
            .iter()
            .map(|&p| {
                asset
                    .values_of(p, Prop::STEPS)
                    .into_iter()
                    .map(|step| {
                        let mode = match asset.values_of(step, Prop::DATA_MODE).first() {
                            Some(&m) => match asset.spans[m].record {
                                Record::Value(Value::Enum { value, .. } | Value::U32(value)) => {
                                    value
                                }
                                _ => return None,
                            },
                            None => 0,
                        };
                        let target = match asset.values_of(step, Prop::TARGET_WIDGET).first() {
                            Some(&t) => match asset.spans[t].record {
                                Record::Ref(r) => Some(r.id),
                                Record::Null => None,
                                _ => return None,
                            },
                            None => None,
                        };
                        Some((mode, target))
                    })
                    .collect::<Option<Vec<Step>>>()
            })
            .collect();
        Ok(SlotPlan {
            asset,
            widgets,
            table: asset.bindings(widgets),
            tables,
            steps,
            new_p: Vec::new(),
            new_t: Vec::new(),
            new_c: Vec::new(),
            new_providers: Vec::new(),
            uses: HashMap::new(),
        })
    }

    /// The slot for `slot`: an existing one (one more use) or a new one.
    pub(super) fn resolve(&mut self, slot: &NewSlot) -> anyhow::Result<At> {
        use anyhow::ensure;
        let (np, nt) = (
            self.tables.containers.1.len(),
            self.tables.templates.1.len(),
        );
        Ok(match slot {
            NewSlot::Existing(v) => {
                ensure!(*v < self.table.slots.len(), "no slot {v} to share");
                match self.share(*v as u32) {
                    Some(at) => at,
                    None if *v < np => At::P(*v),
                    None => At::T(*v - np),
                }
            }
            NewSlot::Widget { widget, path } => {
                ensure!(!path.is_empty(), "a widget slot needs a property");
                let same = |s: &Slot| match s {
                    Slot::Widget { widget: w, path: p } => {
                        (*w == Some(*widget) || (*widget == 0 && w.is_none())) && p == path
                    }
                    _ => false,
                };
                if let Some(i) = self.table.slots[..np].iter().position(same) {
                    *self.uses.entry(self.tables.containers.1[i]).or_default() += 1;
                    return Ok(At::P(i));
                }
                let step = match *widget {
                    0 => (1, None),
                    w => (0, Some(self.widget_id(w)?)),
                };
                self.container(vec![step], path)
            }
            NewSlot::Clip { clip, path } => {
                ensure!(!path.is_empty(), "a clip slot needs a property");
                let same = |s: &Slot| matches!(s, Slot::Clip { clip: c, path: p } if c == clip && p == path);
                if let Some(i) = self.table.slots[..np].iter().position(same) {
                    *self.uses.entry(self.tables.containers.1[i]).or_default() += 1;
                    return Ok(At::P(i));
                }
                let id = self
                    .asset
                    .clip_ids()
                    .get(*clip)
                    .copied()
                    .flatten()
                    .ok_or_else(|| anyhow::anyhow!("that clip has no id to bind to"))?;
                self.container(vec![(2, Some(id))], path)
            }
            NewSlot::NestedClip { widget, clip, path } => {
                ensure!(!path.is_empty(), "a clip slot needs a property");
                let same = |s: &Slot| {
                    matches!(s, Slot::Nested { widget: w, inner, path: p }
                        if w == widget && inner[..] == [*clip] && p == path)
                };
                if let Some(i) = self.table.slots[..np].iter().position(same) {
                    *self.uses.entry(self.tables.containers.1[i]).or_default() += 1;
                    return Ok(At::P(i));
                }
                let container = self.widget_id(*widget)?;
                self.container(vec![(0, Some(container)), (2, Some(*clip))], path)
            }
            NewSlot::Template(i, field) => {
                ensure!(
                    *i < self.asset.properties().len(),
                    "no such template property"
                );
                if let Some(k) =
                    (np..np + nt).find(|&k| self.table.slots[k] == Slot::Template(*i, *field))
                {
                    *self
                        .uses
                        .entry(self.tables.templates.1[k - np])
                        .or_default() += 1;
                    return Ok(At::T(k - np));
                }
                let mut c = Vec::new();
                super::new_object(&mut c, ObjectId::None, Class::TEMPLATE_CONTAINER);
                c.extend_from_slice(&(*i as u32).to_le_bytes());
                c.extend_from_slice(&field.to_le_bytes());
                c.extend_from_slice(&1u32.to_le_bytes());
                self.new_t.push(c);
                At::T(nt + self.new_t.len() - 1)
            }
            NewSlot::Copied(bytes) => {
                self.new_c.push(bytes.clone());
                At::C(self.tables.constants.1.len() + self.new_c.len() - 1)
            }
            NewSlot::Constant { kind, value } => {
                let mut c = Vec::new();
                super::new_object(&mut c, ObjectId::None, Class::CONSTANT);
                c.extend_from_slice(&kind.to_le_bytes());
                c.extend_from_slice(&[0; 16]);
                value.encode(&mut c);
                self.new_c.push(c);
                At::C(self.tables.constants.1.len() + self.new_c.len() - 1)
            }
        })
    }

    /// Widget `w`'s object id, which provider steps name.
    fn widget_id(&self, w: usize) -> anyhow::Result<Guid> {
        match self
            .widgets
            .get(w)
            .map(|w| &self.asset.spans[w.object].record)
        {
            Some(Record::Object(o)) => match o.id {
                ObjectId::Id(g) => Ok(g),
                _ => anyhow::bail!("that widget has no id to bind to"),
            },
            _ => anyhow::bail!("no such widget"),
        }
    }

    /// A new PropertyContainer for `path` through the provider with
    /// `steps`: the file's own when one has the same targets (a widget's or
    /// the root's one step whatever its DataMode), else a new one.
    fn container(&mut self, steps: Vec<Step>, path: &[(u32, u32)]) -> At {
        let targets = |s: &[Step]| s.iter().map(|x| x.1).collect::<Vec<_>>();
        let want = targets(&steps);
        let provider = match self
            .steps
            .iter()
            .position(|s| s.as_deref().is_some_and(|s| targets(s) == want))
        {
            Some(p) => p,
            None => match self.new_providers.iter().position(|s| targets(s) == want) {
                Some(k) => self.tables.providers.1.len() + k,
                None => {
                    self.new_providers.push(steps);
                    self.tables.providers.1.len() + self.new_providers.len() - 1
                }
            },
        };
        let mut c = Vec::new();
        super::new_object(&mut c, ObjectId::None, Class::PROPERTY_CONTAINER);
        crate::varint::write(&mut c, path.len() as u32);
        for &(field, uid) in path {
            super::new_object(&mut c, ObjectId::None, Class::PROPERTY_ID);
            c.extend_from_slice(&field.to_le_bytes());
            c.extend_from_slice(&uid.to_le_bytes());
        }
        c.extend_from_slice(&(provider as u32).to_le_bytes());
        c.extend_from_slice(&1u32.to_le_bytes());
        self.new_p.push(c);
        At::P(self.tables.containers.1.len() + self.new_p.len() - 1)
    }

    /// One more use of existing slot `v` (a binding copy sharing it): a
    /// container's or template container's UseCount goes up; a constant is
    /// copied instead, one per binding as the game's files have them.
    pub(super) fn share(&mut self, v: u32) -> Option<At> {
        let (np, nt) = (
            self.tables.containers.1.len(),
            self.tables.templates.1.len(),
        );
        let v = v as usize;
        if v < np {
            *self.uses.entry(self.tables.containers.1[v]).or_default() += 1;
            None
        } else if v < np + nt {
            *self
                .uses
                .entry(self.tables.templates.1[v - np])
                .or_default() += 1;
            None
        } else {
            let c = *self.tables.constants.1.get(v - np - nt)?;
            self.new_c
                .push(self.asset.bytes[self.asset.subtree_bytes(c)].to_vec());
            Some(At::C(self.tables.constants.1.len() + self.new_c.len() - 1))
        }
    }

    /// Where stored slot index `v` goes once the planned slots are in:
    /// new containers go after the existing ones, new template containers
    /// after theirs, pushing the later regions along.
    pub(super) fn renumber(&self, v: u32) -> u32 {
        let (np, nt) = (
            self.tables.containers.1.len() as u32,
            self.tables.templates.1.len() as u32,
        );
        let (dp, dt) = (self.new_p.len() as u32, self.new_t.len() as u32);
        match v {
            u32::MAX => v,
            v if v >= np + nt => v + dp + dt,
            v if v >= np => v + dp,
            v => v,
        }
    }

    /// A planned slot's index in the edited file.
    pub(super) fn number(&self, a: &At) -> u32 {
        let (np, nt) = (
            self.tables.containers.1.len() as u32,
            self.tables.templates.1.len() as u32,
        );
        let (dp, dt) = (self.new_p.len() as u32, self.new_t.len() as u32);
        match *a {
            At::P(i) => i as u32,
            At::T(i) => np + dp + i as u32,
            At::C(i) => np + dp + nt + dt + i as u32,
        }
    }

    /// The splices adding the planned slots, renumbering the stored slot
    /// indices they move and counting the uses added, and the array count
    /// changes.
    pub(super) fn finish(self) -> (Vec<super::Splice>, HashMap<usize, i64>) {
        let asset = self.asset;
        let mut splices = Vec::new();
        let mut counts = HashMap::new();
        if !self.new_p.is_empty() || !self.new_t.is_empty() {
            let shift = |v: u32| self.renumber(v);
            for (i, s) in asset.spans.iter().enumerate() {
                let (Some(f), Record::Value(Value::U32(v))) = (s.field, &s.record) else {
                    continue;
                };
                let Record::Object(h) = &asset.spans[f.object].record else {
                    continue;
                };
                let crc = asset
                    .class_props(h.class)
                    .and_then(|p| p.get(usize::from(f.prop) - 1))
                    .map(|p| p.name_crc);
                let indexes = match crc {
                    Some(Prop::BINDING_SOURCES | Prop::BINDING_TARGETS) => {
                        h.class == Class::BINDING
                    }
                    Some(Prop::TRACK_TARGET) => true,
                    _ => false,
                };
                if indexes && shift(*v) != *v {
                    splices.push(super::Splice {
                        range: asset.spans[i].range.clone(),
                        bytes: shift(*v).to_le_bytes().to_vec(),
                    });
                }
            }
        }
        for (object, more) in &self.uses {
            let Some(&span) = asset.values_of(*object, Prop::USE_COUNT).first() else {
                continue;
            };
            if let Record::Value(Value::U32(n)) = asset.spans[span].record {
                splices.push(super::Splice {
                    range: asset.spans[span].range.clone(),
                    bytes: (n + more).to_le_bytes().to_vec(),
                });
            }
        }
        let providers = self
            .new_providers
            .iter()
            .map(|steps| {
                let mut p = Vec::new();
                super::new_object(&mut p, ObjectId::None, Class::PROVIDER);
                crate::varint::write(&mut p, steps.len() as u32);
                for &(mode, target) in steps {
                    super::new_object(&mut p, ObjectId::None, Class::STEP);
                    p.extend_from_slice(&mode.to_le_bytes());
                    match target {
                        None => p.push(ObjectId::NULL),
                        Some(id) => super::Reference {
                            class: Class::WIDGET_REF,
                            id,
                        }
                        .encode(&mut p),
                    }
                }
                p
            })
            .collect();
        let Tables {
            containers,
            templates,
            constants,
            providers: provider_array,
            ..
        } = &self.tables;
        append(asset, &mut splices, &mut counts, containers, self.new_p);
        append(asset, &mut splices, &mut counts, templates, self.new_t);
        append(asset, &mut splices, &mut counts, constants, self.new_c);
        append(asset, &mut splices, &mut counts, provider_array, providers);
        (splices, counts)
    }
}

impl Asset {
    fn binding_tables(&self, root: usize) -> anyhow::Result<Tables> {
        let get = |crc: u32, what: &str| {
            self.array(root, crc)
                .ok_or_else(|| anyhow::anyhow!("this file stores no {what} table"))
        };
        Ok(Tables {
            containers: get(Prop::CONTAINERS, "property container")?,
            templates: get(Prop::TEMPLATE_CONTAINERS, "template property container")?,
            constants: get(Prop::CONSTANTS, "constant")?,
            providers: get(Prop::PROVIDERS, "provider")?,
            bindings: get(Prop::BINDINGS, "binding")?,
        })
    }

    /// Adds `b` as one binding: existing slots for the same widget property
    /// or template property are shared (their UseCount goes up), the rest
    /// are added. Converters with BindingMode, IsDynamic and the two counts
    /// only (Assign, LogicalInvert, Equal, IfTrueFalseAssign, the
    /// arithmetic ones…) can be written.
    pub fn add_binding(&self, widgets: &[Widget], b: &NewBinding) -> anyhow::Result<Asset> {
        use anyhow::ensure;
        ensure!(!b.targets.is_empty(), "a binding needs a target");
        ensure!(
            b.targets
                .iter()
                .all(|t| !matches!(t, NewSlot::Constant { .. } | NewSlot::Copied(_))),
            "a constant cannot be a binding's target"
        );
        let props = super::wire_props(b.converter)
            .ok_or_else(|| anyhow::anyhow!("converter {:08x} is unknown", b.converter))?;
        ensure!(
            props.len() == 4,
            "converter {:08x} carries settings the editor cannot write yet",
            b.converter
        );
        let mut plan = SlotPlan::new(self, widgets)?;
        let sources = b
            .sources
            .iter()
            .map(|s| plan.resolve(s))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let targets = b
            .targets
            .iter()
            .map(|s| plan.resolve(s))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let sources: Vec<u32> = sources.iter().map(|a| plan.number(a)).collect();
        let targets: Vec<u32> = targets.iter().map(|a| plan.number(a)).collect();
        let mut binding = Vec::new();
        super::new_object(&mut binding, ObjectId::None, Class::BINDING);
        super::new_object(&mut binding, ObjectId::None, b.converter);
        binding.extend_from_slice(&0u32.to_le_bytes());
        binding.push(u8::from(sources.len() > 2));
        binding.extend_from_slice(&(sources.len() as u32).to_le_bytes());
        binding.extend_from_slice(&(targets.len() as u32).to_le_bytes());
        u32s(&mut binding, &sources);
        u32s(&mut binding, &targets);
        binding.push(0);
        binding.extend_from_slice(&0u32.to_le_bytes());
        binding.extend_from_slice(&[1, 0]);
        binding.extend_from_slice(&Guid::random().0);
        let bindings = plan.tables.bindings.clone();
        let (mut splices, mut counts) = plan.finish();
        append(self, &mut splices, &mut counts, &bindings, vec![binding]);
        self.finish(
            splices,
            counts,
            &[
                Class::PROPERTY_CONTAINER,
                Class::PROPERTY_ID,
                Class::PROVIDER,
                Class::STEP,
                Class::TEMPLATE_CONTAINER,
                Class::CONSTANT,
                Class::BINDING,
                b.converter,
            ],
        )
    }

    /// Adds copies of existing bindings, each as it is (converter and its
    /// settings, mode, enum rules, EnumEntry) under a new ProfileID, with
    /// the slots in [`BindingCopy::slots`] replaced and the others shared
    /// with the original. Duplicating a bound widget copies the bindings
    /// that set it this way: `add_binding` writes only plain converters.
    pub fn copy_bindings(
        &self,
        widgets: &[Widget],
        copies: &[BindingCopy],
    ) -> anyhow::Result<Asset> {
        use anyhow::{bail, ensure};
        let mut plan = SlotPlan::new(self, widgets)?;
        let bindings = plan.tables.bindings.clone();
        // Every slot planned before any is numbered: numbers move as the
        // regions before them grow.
        let mut planned = Vec::with_capacity(copies.len());
        for c in copies {
            let &obj = bindings
                .1
                .get(c.binding)
                .ok_or_else(|| anyhow::anyhow!("no binding {}", c.binding))?;
            let values: Vec<usize> = self
                .values_of(obj, Prop::BINDING_SOURCES)
                .into_iter()
                .chain(self.values_of(obj, Prop::BINDING_TARGETS))
                .collect();
            let sources = self.values_of(obj, Prop::BINDING_SOURCES).len();
            let mut at: Vec<Option<At>> = Vec::with_capacity(values.len());
            for (k, &span) in values.iter().enumerate() {
                let Record::Value(Value::U32(v)) = self.spans[span].record else {
                    bail!("binding {} has a slot that is not an index", c.binding);
                };
                at.push(match c.slots.iter().find(|(i, _)| *i == k) {
                    Some((_, slot)) => {
                        ensure!(
                            k < sources
                                || !matches!(slot, NewSlot::Constant { .. } | NewSlot::Copied(_)),
                            "a constant cannot be a binding's target"
                        );
                        Some(plan.resolve(slot)?)
                    }
                    None if v == u32::MAX => None,
                    None => plan.share(v),
                });
            }
            ensure!(
                c.slots.iter().all(|(k, _)| *k < values.len()),
                "binding {} has fewer slots than the copy names",
                c.binding
            );
            planned.push((obj, values, at));
        }
        let mut items = Vec::with_capacity(planned.len());
        for (obj, values, at) in planned {
            let range = self.subtree_bytes(obj);
            let base = range.start;
            let mut bytes = self.bytes[range].to_vec();
            for (&span, a) in values.iter().zip(&at) {
                let Record::Value(Value::U32(v)) = self.spans[span].record else {
                    unreachable!("checked above");
                };
                let v = a
                    .as_ref()
                    .map_or_else(|| plan.renumber(v), |a| plan.number(a));
                let r = &self.spans[span].range;
                bytes[r.start - base..r.end - base].copy_from_slice(&v.to_le_bytes());
            }
            if let Some(&id) = self.values_of(obj, Prop::PROFILE_ID).first() {
                let r = &self.spans[id].range;
                ensure!(r.len() == 16, "a binding's ProfileID is not 16 bytes");
                bytes[r.start - base..r.end - base].copy_from_slice(&Guid::random().0);
            }
            items.push(bytes);
        }
        let (mut splices, mut counts) = plan.finish();
        append(self, &mut splices, &mut counts, &bindings, items);
        // The copies' own classes (binding, converter, rules) are in the
        // file already: they come from it.
        self.finish(splices, counts, &SlotPlan::CLASSES)
    }

    /// The game data this template's bindings read or write: per data
    /// path, the ObjectTemplate (a game-object class's guid) of the
    /// template property it starts at, and the field ids after it. Across
    /// the install these are the fields known to exist on each class,
    /// with ids that are right for it (the same name hashes differently
    /// in different classes).
    pub fn data_paths(&self, widgets: &[Widget]) -> Vec<(Guid, Vec<u32>)> {
        let table = self.bindings(widgets);
        let mut out: Vec<(Guid, Vec<u32>)> = Vec::new();
        for s in &table.slots {
            let Slot::Widget {
                widget: Some(0) | None,
                path,
            } = s
            else {
                continue;
            };
            let [(_, first), rest @ ..] = &path[..] else {
                continue;
            };
            let Some(class) = self
                .properties()
                .iter()
                .find(|p| p.uid == *first)
                .and_then(|p| p.object_asset)
            else {
                continue;
            };
            // An axis of the last field is the field itself here.
            let fields: Vec<u32> = rest.iter().map(|&(_, id)| id).collect();
            if !fields.is_empty() && !out.iter().any(|(g, f)| *g == class.id && *f == fields) {
                out.push((class.id, fields));
            }
        }
        out
    }

    /// Data objects this template hands to its containers: per binding that
    /// copies a data path whole into a container's template input, the
    /// class the path starts at, the path, the container's template and
    /// the input's uid. With the input's class (a property of that
    /// template) this says what class the object at the path has, so the
    /// fields known on it extend the path.
    pub fn data_feeds(&self, widgets: &[Widget]) -> Vec<(Guid, Vec<u32>, Guid, u32)> {
        let table = self.bindings(widgets);
        let class_of = |uid: u32| {
            self.properties()
                .iter()
                .find(|p| p.uid == uid)
                .and_then(|p| p.object_asset)
                .map(|r| r.id)
        };
        let mut out = Vec::new();
        for b in &table.bindings {
            if !matches!(Converter::of(b.converter), Some(Converter::Assign)) {
                continue;
            }
            let Some(Slot::Widget {
                widget: Some(0) | None,
                path,
            }) = b.sources.first().and_then(|&s| table.slots.get(s))
            else {
                continue;
            };
            let [(_, first), rest @ ..] = &path[..] else {
                continue;
            };
            let Some(class) = class_of(*first) else {
                continue;
            };
            let fields: Vec<u32> = rest.iter().map(|&(_, id)| id).collect();
            for &t in &b.targets {
                if let Some(Slot::Widget {
                    widget: Some(w),
                    path,
                }) = table.slots.get(t)
                    && let [(1, uid)] = path[..]
                    && uid & Slot::BUILTIN == 0
                    && let Some(r) = widgets.get(*w).and_then(|wd| wd.container_template())
                {
                    out.push((class, fields.clone(), r.id, uid));
                }
            }
        }
        out
    }

    /// Removes binding `index` (of [`Bindings::bindings`]). Its slots stay
    /// in their tables, one use fewer: removing them would renumber the
    /// rest, and an unused slot does nothing.
    /// Feeds source `k` of binding `index` from `slot` instead, in place:
    /// the binding keeps its place in the list, its ProfileID, mode and
    /// converter settings (removing and adding it anew moved it to the end
    /// and lost what `add_binding` cannot write). The new slot is shared or
    /// added; the old one's UseCount goes down, as `remove_binding` does.
    pub fn set_binding_source(
        &self,
        widgets: &[Widget],
        index: usize,
        k: usize,
        slot: &NewSlot,
    ) -> anyhow::Result<Asset> {
        let mut plan = SlotPlan::new(self, widgets)?;
        let &object = plan
            .tables
            .bindings
            .1
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("no binding {index}"))?;
        let &span = self
            .values_of(object, Prop::BINDING_SOURCES)
            .get(k)
            .ok_or_else(|| anyhow::anyhow!("binding {index} has no source {k}"))?;
        let Record::Value(Value::U32(old)) = self.spans[span].record else {
            anyhow::bail!("binding {index}'s source {k} is not an index");
        };
        let (containers, templates) = (
            plan.tables.containers.1.clone(),
            plan.tables.templates.1.clone(),
        );
        let at = plan.resolve(slot)?;
        let new = plan.number(&at);
        let (mut splices, counts) = plan.finish();
        // `finish` renumbers every stored index past the slots it adds,
        // this one included; it gets the new slot's number instead.
        let range = self.spans[span].range.clone();
        splices.retain(|s| s.range != range);
        splices.push(super::Splice {
            range,
            bytes: new.to_le_bytes().to_vec(),
        });
        let old = old as usize;
        let container = containers
            .get(old)
            .or_else(|| templates.get(old.wrapping_sub(containers.len())));
        if let Some(&c) = container
            && let Some(&count) = self.values_of(c, Prop::USE_COUNT).first()
            && let Record::Value(Value::U32(n)) = self.spans[count].record
        {
            let range = self.spans[count].range.clone();
            anyhow::ensure!(
                !splices.iter().any(|s| s.range == range),
                "the old and the new source are one slot"
            );
            splices.push(super::Splice {
                range,
                bytes: n.saturating_sub(1).to_le_bytes().to_vec(),
            });
        }
        self.finish(splices, counts, &SlotPlan::CLASSES)
    }

    pub fn remove_binding(&self, widgets: &[Widget], index: usize) -> anyhow::Result<Asset> {
        let root = widgets
            .first()
            .map(|w| w.object)
            .ok_or_else(|| anyhow::anyhow!("no widgets"))?;
        let Tables {
            containers,
            templates,
            bindings,
            ..
        } = self.binding_tables(root)?;
        let &object = bindings
            .1
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("no binding {index}"))?;
        let table = self.bindings(widgets);
        let mut splices = vec![super::Splice {
            range: self.subtree_bytes(object),
            bytes: Vec::new(),
        }];
        let mut fewer: HashMap<usize, u32> = HashMap::new();
        let b = &table.bindings[index];
        for &s in b.sources.iter().chain(&b.targets) {
            let container = containers
                .1
                .get(s)
                .or_else(|| templates.1.get(s.wrapping_sub(containers.1.len())));
            if let Some(&c) = container {
                *fewer.entry(c).or_default() += 1;
            }
        }
        for (c, less) in fewer {
            if let Some(&span) = self.values_of(c, Prop::USE_COUNT).first()
                && let Record::Value(Value::U32(n)) = self.spans[span].record
            {
                splices.push(super::Splice {
                    range: self.spans[span].range.clone(),
                    bytes: n.saturating_sub(less).to_le_bytes().to_vec(),
                });
            }
        }
        let mut counts = HashMap::new();
        counts.insert(bindings.0, -1);
        self.finish(splices, counts, &[])
    }
}
