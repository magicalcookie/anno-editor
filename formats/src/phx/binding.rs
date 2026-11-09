// Data bindings of a widget template, and a static evaluation of the plain
// ones, so a layout can show what the game draws before any game data
// arrives.
//
// The root holds the binding tables. A binding (`0xedcf0433`) copies from
// its source slots to its target slots; slot indices run through one table,
// PropertyContainers first, then the template-property containers, then the
// constants (checked on 164,931 indices in the install; no target is ever a
// constant). A PropertyContainer names a property of a widget: a provider
// (a path to a widget, `0x8fda299c`) and a PropertyPath, either one
// built-in widget property id (`0x80000001` Position …) or a component
// instance UniqueId and one of its custom property UniqueIds.
//
// Evaluated here: bindings with BindingMode 0 whose converter is known
// (`Converter`: copies, arithmetic, comparisons, logic, conditionals,
// string joins, enum rules), in file order, swept until nothing changes.
// A few unnamed converters are skipped; their targets keep what they had.
// Slots are resolved once per template (`Prepared`, `Access`), so an
// evaluation reads and writes flat per-place values without allocating.

use std::collections::HashMap;
use std::sync::Arc;

use super::{Asset, DefaultValue, Field, Guid, ObjectId, Record, Value, Widget, name_crc};

const PROP_BINDINGS: u32 = 0xa433_6119;
/// A binding's converter object; the property shares its class's name.
const PROP_CONVERTER: u32 = name_crc(b"ConverterInstance");
const PROP_CONTAINERS: u32 = name_crc(b"PropertyContainers");
const PROP_TEMPLATE_CONTAINERS: u32 = name_crc(b"CustomPropertyContainers");
const PROP_CONSTANTS: u32 = name_crc(b"ConstantPropertyContainers");
const PROP_PROVIDERS: u32 = name_crc(b"DataProviderContainers");
const PROP_PATH: u32 = name_crc(b"PropertyPath");
const PROP_FIELD: u32 = name_crc(b"PropertyField");
const PROP_UNIQUE_ID: u32 = name_crc(b"UniqueId");
const PROP_PROVIDER: u32 = name_crc(b"DataProviderIndex");
const PROP_TEMPLATE_INDEX: u32 = name_crc(b"CustomPropertyIndex");
const PROP_DEFAULT: u32 = name_crc(b"DefaultValue");
const PROP_PROPERTY_TYPE: u32 = name_crc(b"PropertyType");
const PROP_STEPS: u32 = name_crc(b"DataProviderPath");
const PROP_TARGET_WIDGET: u32 = name_crc(b"DataProviderObject");
/// A provider step's DataMode: 0 a widget, 1 the root, 2 a clip.
const PROP_DATA_MODE: u32 = name_crc(b"DataMode");
const PROP_SOURCES: u32 = 0x28c3_8074;
const PROP_TARGETS: u32 = 0xcec4_084b;
const PROP_BINDING_MODE: u32 = name_crc(b"BindingMode");
const PROP_BINDING_INFOS: u32 = name_crc(b"BindingInfos");
const PROP_RULE_ENTRY: u32 = name_crc(b"EnumValueId");
const PROP_RULE_TARGET: u32 = name_crc(b"TargetIndex");
const PROP_ENUM_ASSET: u32 = name_crc(b"EnumAsset");

/// The Positioning ComponentTemplate: native code sets its Output to its
/// Input converted (`ConversionType`, halo.PositionConversionType) from
/// the widget's own space to the screen or back. Bindings chain two to
/// place a widget where another one is (the build panel's header glow
/// under its header).
/// TODO(alex): GlobalConversionType (relative to the window or to the
/// scene) is not told apart; every scene here fills the window.
const POSITIONING: Guid = Guid::from_const("01e5a43d-a90d-4d06-b7a9-f42b90d658ab");
const POSITIONING_CONVERSION: u32 = 0x7f2c_6701;
const POSITIONING_INPUT: u32 = 0x7fde_2700;
const POSITIONING_OUTPUT: u32 = 0x7436_492f;
/// A widget's DataSource (interactive WidgetType base): the game data its
/// list or template content shows.
const DATA_SOURCE: u32 = 0x3000_0001;
/// ValueComponent (`cf1d5ae2…`, native `CValueCompare`): MyValue,
/// MinValue, MaxValue (Vector3). The native update takes the per-axis
/// minimum and maximum of MyValue over
/// the instances under the same container and writes them into every
/// one's MinValue and MaxValue: list items share their widest width (the
/// quest objectives' -SizeInX, the hippodrome tabs' biggest size).
// TODO(alex): one instance is evaluated at a time here; the stored value
// stands in for its siblings (see `value_component`).
const VALUE_COMPONENT: Guid = Guid::from_const("cf1d5ae2-e3b0-41ac-b526-3b90b7da1f83");
const VALUE_MY: u32 = 0x7f71_ffa6;
const VALUE_MIN: u32 = 0x77a5_1626;
const VALUE_MAX: u32 = 0x72e7_7949;
const LOCAL_TO_GLOBAL: u32 = 0x54db_c889;
const GLOBAL_TO_LOCAL: u32 = 0x5f41_8813;
const PROP_RULE_ID: u32 = name_crc(b"RuleId");
const PROP_CONVERSION_RULES: u32 = name_crc(b"ConversionRules");
const PROP_CONVERSION_VALUES: u32 = name_crc(b"ConversionValues");
/// ConversionValue's enum entry UniqueId.
const PROP_CONVERSION_ENTRY: u32 = name_crc(b"EnumEntryUniqueId");
const PROP_VALUE: u32 = name_crc(b"Value");

/// Built-in widget property ids (PropertyId UniqueId without the high bit;
/// names from the components' AffectedProperties).
pub mod builtin {
    pub const POSITION: u32 = 1;
    pub const PIVOT: u32 = 2;
    pub const ROTATION: u32 = 3;
    pub const SIZE: u32 = 4;
    pub const COLOR: u32 = 5;
    /// A label's Text (String sources; images use 42 for theirs).
    pub const TEXT: u32 = 8;
    /// A label's LocalizationId (`"A018 1934 A893 EEE1"`), read from use:
    /// the text atom binds its data's `ID` here next to `Text` (8), and
    /// the game shows the line it names (the archive's "!Archive" header
    /// reads "Archive").
    pub const LOCALIZATION_ID: u32 = 0x0a;
    /// An image widget's image as a path String (Enum sources pick one).
    pub const IMAGE: u32 = 42;
    pub const ALPHA: u32 = 6;
    pub const VISIBLE: u32 = 7;
    pub const SCALE: u32 = 0x35;
    /// A label's DisplayedSize: its text's laid-out size. Read from use:
    /// the HUD icon-text copies it into the label's TextPlane and centres
    /// icon + text in its slot with it.
    pub const DISPLAYED_SIZE: u32 = 0x52;
    /// A label's TextPlane: the box its text wraps in.
    pub const TEXT_PLANE: u32 = 0x0f;
    /// Label Horizontal/VerticalSizing, AlignmentX/Y and Horizontal/
    /// VerticalOverflow, from the text atom template that forwards its
    /// `-Layout` object's Sizing_X…Overflow_Y fields to them.
    pub const SIZING: [u32; 2] = [0x55, 0x56];
    pub const ALIGNMENT: [u32; 2] = [0x61, 0x62];
    pub const OVERFLOW: [u32; 2] = [0x63, 0x64];
    /// A label's LineSpacing, read from use: the text atom binds its
    /// `-Properties` spacing here (1.2 on the patched quest objectives).
    pub const LINE_SPACING: u32 = 0x67;
    /// An image's UVOffset and UVScale (the governors bar's hairlines
    /// mirror their texture with UVScale.x −1).
    pub const UV_OFFSET: u32 = 0x38;
    pub const UV_SCALE: u32 = 0x39;
    pub const Z_INDEX: u32 = 0x46;
    /// A root event that fires once the widget is shown, read from use:
    /// templates pass it on to their children and to animation
    /// dispatchers, and the construction menu hides its placeholder disc
    /// on it.
    pub const SHOWN: u32 = 0x3c;
    /// An animated template's actions and the events it sends, from the
    /// native property table (0x27 has no name there).
    pub const START: u32 = 0x1f;
    pub const STOP: u32 = 0x20;
    pub const PAUSE: u32 = 0x21;
    pub const RESUME: u32 = 0x22;
    pub const STARTED: u32 = 0x23;
    pub const STOPPED: u32 = 0x24;
    pub const PAUSED: u32 = 0x25;
    pub const RESUMED: u32 = 0x26;
    pub const COMPLETED: u32 = 0x28;

    /// The built-ins by name, as the components' AffectedProperties and
    /// the native property table spell them. The editor names pins,
    /// timeline lanes and binder targets from this.
    // TODO(alex): LocalizationId (0x0a) and LineSpacing (0x67) are known
    // from use only; they still read as `builtin 0xa` in the editor.
    const NAMES: [(u32, &str); 31] = [
        (POSITION, "Position"),
        (PIVOT, "PivotPoint"),
        (ROTATION, "RotationAngle"),
        (SIZE, "Size"),
        (COLOR, "ColorTint"),
        (ALPHA, "AlphaTint"),
        (VISIBLE, "Visible"),
        (SCALE, "Scale"),
        (TEXT, "Text"),
        (IMAGE, "Image"),
        (DISPLAYED_SIZE, "DisplayedSize"),
        (UV_OFFSET, "UVOffset"),
        (UV_SCALE, "UVScale"),
        (Z_INDEX, "ZIndex"),
        (TEXT_PLANE, "TextPlane"),
        (SHOWN, "Shown"),
        (START, "Start"),
        (STOP, "Stop"),
        (PAUSE, "Pause"),
        (RESUME, "Resume"),
        (STARTED, "Started"),
        (STOPPED, "Stopped"),
        (PAUSED, "Paused"),
        (RESUMED, "Resumed"),
        (COMPLETED, "Completed"),
        (SIZING[0], "HorizontalSizing"),
        (SIZING[1], "VerticalSizing"),
        (ALIGNMENT[0], "AlignmentX"),
        (ALIGNMENT[1], "AlignmentY"),
        (OVERFLOW[0], "HorizontalOverflow"),
        (OVERFLOW[1], "VerticalOverflow"),
    ];

    /// Built-in `id`'s name (the id without its high bit): "Position",
    /// "Completed".
    pub fn name(id: u32) -> Option<&'static str> {
        NAMES.iter().find(|n| n.0 == id).map(|n| n.1)
    }
}
const BUILTIN: u32 = 0x8000_0000;

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

/// A container input fed from game data: its UniqueId and the data path it
/// comes from (empty when not a data path).
pub type GameFed = (u32, Vec<(u32, u32)>);

/// A write a template's binding makes into a widget of a nested template:
/// the remaining widget ids (outermost first), the property path, the value.
pub type NestedWrite = (Vec<Guid>, Vec<(u32, u32)>, DefaultValue);

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
/// (see `CONVERTERS`); designers see a looser name on the template property
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
    /// builtin event `0x3c` ([`builtin::SHOWN`]); hover, focus and value
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

/// Converter classes the editor evaluates.
const CONVERTERS: [(u32, Converter); 46] = {
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

impl Converter {
    pub fn of(class: u32) -> Option<Converter> {
        CONVERTERS.iter().find(|c| c.0 == class).map(|c| c.1)
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

/// The SceneTransitionObject's show event (the exe names it ShowSingle;
/// Lua registers it as Show): the game fires it as a scene opens. Read from
/// use: in the shared transition template `f778e8bd…` it Starts
/// ShowAnimation, `0x254fe1b9` (HideSingle) Starts HideAnimation.
const SCENE_TRANSITION_SHOW: u32 = 0x29d4_3da4;

impl Bindings {
    /// When a source fires as the screen opens: 0 for the root's SHOWN
    /// event, 1 for the scene transition's Show, which follows it (the
    /// diplomacy screen's SHOWN Starts a `Default` clip at alpha 0, Show
    /// then Starts `OpeningAnim`).
    fn fires_on_open(&self, source: usize) -> Option<u8> {
        match self.slots.get(source) {
            Some(Slot::Widget {
                widget: Some(0) | None,
                path,
            }) => {
                if path[..] == [(1, BUILTIN | builtin::SHOWN)] {
                    Some(0)
                } else if path.last().is_some_and(|p| p.1 == SCENE_TRANSITION_SHOW) {
                    Some(1)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Targets of bindings that fire as the screen opens, in firing order.
    fn on_open(&self) -> Vec<&Slot> {
        let mut out: Vec<(u8, &Slot)> = self
            .bindings
            .iter()
            // Plain copies only: a button's `IfTrueAssign [Shown, IsSelected]
            // → Selected · Start` waits on a state the editor does not know.
            .filter(|b| {
                matches!(
                    Converter::of(b.converter),
                    Some(Converter::Assign | Converter::EventAssign)
                )
            })
            .filter_map(|b| Some((self.fires_on_open(*b.sources.first()?)?, b)))
            .flat_map(|(when, b)| {
                b.targets
                    .iter()
                    .filter_map(move |&t| Some((when, self.slots.get(t)?)))
            })
            .collect();
        out.sort_by_key(|x| x.0);
        out.into_iter().map(|x| x.1).collect()
    }

    /// This file's clips (indices into [`Asset::clips`]) that opening
    /// Starts, in the order they start.
    pub fn clips_started_on_open(&self) -> Vec<usize> {
        let start = [(1, BUILTIN | builtin::START)];
        let mut out: Vec<usize> = Vec::new();
        for s in self.on_open() {
            if let Slot::Clip { clip, path } = s
                && path[..] == start
                && !out.contains(clip)
            {
                out.push(*clip);
            }
        }
        out
    }

    /// This file's clips that a binding Starts from the file's own state
    /// (a template property, a widget, game data), not from opening or from
    /// another clip's event: they play when that state changes, whatever
    /// their name says (the construction menu's `BuildMode_In`, started when
    /// build mode turns on). With each: the template properties (indices
    /// into [`Asset::properties`]) whose turning true starts it
    /// (FalseToTrueChange), which fire at opening when the property starts
    /// out true (the archive panel's `Archive_In`, its PanelType flag on).
    pub fn clips_started_by_state(&self) -> Vec<(usize, Vec<usize>)> {
        let start = [(1, BUILTIN | builtin::START)];
        let opening = self.clips_started_on_open();
        let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
        for b in &self.bindings {
            let from_state = b.sources.iter().any(|&s| {
                matches!(
                    self.slots.get(s),
                    Some(Slot::Template(..) | Slot::Widget { .. })
                )
            }) && b
                .sources
                .first()
                .is_none_or(|&s| self.fires_on_open(s).is_none());
            if !from_state {
                continue;
            }
            const FALSE_TO_TRUE: u32 = name_crc(b"FalseToTrueChangeConverterInstance");
            let trigger = match (
                b.converter,
                b.sources.first().and_then(|&s| self.slots.get(s)),
            ) {
                (FALSE_TO_TRUE, Some(Slot::Template(i, 1))) => Some(*i),
                _ => None,
            };
            for &t in &b.targets {
                if let Some(Slot::Clip { clip, path }) = self.slots.get(t)
                    && path[..] == start
                    && !opening.contains(clip)
                {
                    let at = match out.iter().position(|(c, _)| c == clip) {
                        Some(at) => at,
                        None => {
                            out.push((*clip, Vec::new()));
                            out.len() - 1
                        }
                    };
                    out[at].1.extend(trigger);
                }
            }
        }
        out
    }

    /// (container widget, clip guid in its template) pairs whose Start this
    /// file fires on opening (a popup's root: SHOWN → its transition
    /// template's ShowAnimation).
    pub fn nested_clips_started_on_open(&self) -> Vec<(usize, Guid)> {
        let start = [(1, BUILTIN | builtin::START)];
        self.on_open()
            .into_iter()
            .filter_map(|s| match s {
                Slot::Nested {
                    widget,
                    inner,
                    path,
                } if path[..] == start => Some((*widget, *inner.last()?)),
                _ => None,
            })
            .collect()
    }
}

/// Everything evaluation wrote: per widget, the bound geometry and a bound
/// label text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evaluated {
    pub bound: Vec<Bound>,
    /// Per widget: the String a binding put in as its content: a label's
    /// Text (8), or an image widget's image path (42, `data\ui\…\x.png`).
    pub text: Vec<Option<String>>,
    /// Per widget: the LocalizationId a binding put on a label.
    pub loc: Vec<Option<String>>,
    /// Per widget: values bindings wrote into a container's template
    /// properties (one non-built-in id), by property UniqueId. They are that
    /// template's inputs.
    pub inputs: Vec<HashMap<u32, DefaultValue>>,
    /// Per widget: container template properties a binding feeds from data
    /// the game supplies (a provider path or data-context field that does
    /// not resolve here). The value stored on the instance is only the
    /// authoring state there. With each: the data path it is fed from
    /// (empty when the failing source is not a data path), which names the
    /// input for sample data inside the template.
    pub runtime: Vec<Vec<GameFed>>,
    /// Per container widget: writes into widgets of its template (and
    /// further down) to pass on, see [`NestedWrite`].
    pub nested: Vec<Vec<NestedWrite>>,
    /// The template's own properties as evaluated, by UniqueId: what a
    /// parent reads back as this instance's outputs (`OutputValue-`).
    pub template: Arc<HashMap<u32, DefaultValue>>,
    /// Component properties bindings wrote: (widget, component instance
    /// UniqueId, custom property UniqueId) → value (a stack's alignment
    /// and spacing).
    pub components: HashMap<(usize, u32, u32), DefaultValue>,
    /// What each binding of the file read and wrote on its last run.
    pub trace: Arc<Traces>,
}

/// Per binding of a file (index into [`Bindings::bindings`]): what it read
/// and wrote on its last run, as evaluation left it; the editor asks for
/// one at a time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Traces {
    args: Vec<Val>,
    /// Per binding: where its arguments start in `args`, how many, and the
    /// value; None when it never ran.
    runs: Vec<Option<(u32, u8, Option<Val>)>>,
}

impl Traces {
    /// None when the binding never ran (a source nothing supplies, a mode
    /// the editor does not evaluate).
    pub fn get(&self, binding: usize) -> Option<Trace> {
        let (at, n, value) = self.runs.get(binding)?.as_ref()?;
        let at = *at as usize;
        Some(Trace {
            args: self.args[at..at + *n as usize]
                .iter()
                .map(Val::dv)
                .collect(),
            value: value.as_ref().map(Val::dv),
        })
    }
}

/// One binding's last evaluation, for the editor's binding view.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    pub args: Vec<DefaultValue>,
    /// None: the converter gave nothing for these inputs (a false
    /// IfTrueAssign condition, a type it cannot combine).
    pub value: Option<DefaultValue>,
}

/// What [`evaluate`] reads from a file that does not change between its
/// instances: binding tables compiled to [`Access`]es, widget ids, stored
/// geometry and components.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub table: Bindings,
    ids: HashMap<Guid, usize>,
    geometry: Vec<super::Geometry>,
    pub components: Vec<Vec<super::layout::Component>>,
    /// Per widget: the template property values a container instance
    /// stores (one-id PropertyValues paths), with its CurrentEnumValue.
    pub stored_inputs: Vec<HashMap<u32, DefaultValue>>,
    /// Per widget: the template a container instance expands.
    pub templates: Vec<Option<Guid>>,
    /// Containers whose outputs a binding reads (writing their inputs does
    /// not): all of [`Feed::outputs`] evaluation depends on. Sorted.
    pub(super) output_sources: Vec<usize>,
    /// Widgets whose laid-out box evaluation can observe: a binding reads
    /// their Size, DisplayedSize or Scale (under a bound Size, that over the
    /// laid size), or something writes one axis of their Size (the other
    /// stays as laid out); all of [`Feed::sizes`] evaluation depends on. A
    /// Size written whole is not read. Sorted.
    pub(super) box_readers: Vec<usize>,
    /// Widgets with a Positioning component whose Output a binding reads:
    /// all of [`Feed::transforms`] evaluation depends on. Sorted.
    pub(super) positioned: Vec<usize>,
    /// (Input place, Output place) of those components: a write to the
    /// input changes what the output reads.
    converts: Vec<(u32, u32)>,
    /// Per slot: the place it reads and writes (a widget's property, a
    /// component property, a template property), numbered densely; slots
    /// no binding can read back (game data, nested, clips) have none.
    places: Vec<Option<u32>>,
    /// Per place: what it is; `place_ids` the other way.
    keys: Vec<Place>,
    place_ids: super::layout::FastMap<Place, u32>,
    /// Per slot: how evaluation reads and writes it.
    access: Vec<Access>,
    /// Game-data paths slots read, each once.
    pub(super) paths: Vec<Vec<(u32, u32)>>,
    /// Per `paths` entry: the widget it is read on (0 for the root and
    /// unresolved providers), so a container's field keeps its own slot.
    path_owner: Vec<usize>,
    /// Per binding: its converter, None when the editor does not run it.
    converters: Vec<Option<Converter>>,
    /// Per binding: where its last arguments go in a trace buffer, which
    /// holds `max(1, sources)` of them.
    arg_at: Vec<u32>,
    /// The styles this template's widgets are tagged with (`EnumEntry`).
    styles: Vec<u32>,
    /// Per place: the bindings that write it, in file order.
    writers: Vec<Vec<u32>>,
    /// Bindings that read or write a place more than one binding writes
    /// (a reader between two writers sees the first one's value each
    /// sweep); per binding, the groups it is in.
    groups: Vec<Vec<u32>>,
    group_of: Vec<Vec<u32>>,
    /// [`Bindings::clips_started_on_open`] and
    /// [`Bindings::nested_clips_started_on_open`], once per template.
    pub opening_clips: Vec<usize>,
    /// See [`Bindings::clips_started_by_state`].
    pub state_clips: Vec<(usize, Vec<usize>)>,
    pub opening_nested: Vec<(usize, Guid)>,
    /// Widgets that start hidden whatever they store: scroll bars, which
    /// the game shows only once their list's content overflows (see
    /// [`Prepared::start_hidden`]).
    pub idle_hidden: Vec<usize>,
    /// Container widgets whose template is an InfoTipArea hover zone (see
    /// `layout::INFOTIP_AREA`).
    pub hover_zones: Vec<usize>,
    /// Per place: container inputs bindings read before anything writes
    /// them, and those written one axis at a time, from what instances
    /// store and the templates' defaults; made on the first evaluation.
    container_defaults: std::sync::OnceLock<ContainerDefaults>,
    /// (widget, property) pairs a binding writes on a widget, whole or
    /// a field of it (a container's template input). Sorted.
    input_writes: Vec<(usize, u32)>,
    /// Fields of a container's data-object inputs its instance stores
    /// (`-Header.Text = "!Archive"` on the archive's popup): passed to the
    /// template as a parent's field writes are.
    stored_fields: Vec<(usize, Vec<(u32, u32)>, DefaultValue)>,
    /// ValueComponent (MyValue place, Min/MaxValue place, whether Max,
    /// the stored output value).
    value_outputs: Vec<(u32, u32, bool, Option<DefaultValue>)>,
}

/// See `Prepared::container_defaults`: per place, the value read before a
/// write, and the value an axis write starts from.
type ContainerDefaults = (Vec<Option<Val>>, Vec<Option<Val>>);

/// A place bindings read and write: axes of a vector share their place;
/// DisplayedSize and Scale read what Size writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Place {
    /// A built-in property, or a container's template property.
    Widget(usize, u32),
    Component(usize, u32, u32),
    Template(usize),
}

/// Where a template's outputs keep field `field` of its object property
/// `object` (written as a component path on its root); field 0 lists the fields written. A
/// mixed key, not a UniqueId: one landing on a real property of the same
/// template is a 1 in 2^32 per pair.
fn field_key(object: u32, field: u32) -> u32 {
    object.rotate_left(16) ^ field ^ 0x9e37_79b9
}

fn place_of(s: &Slot) -> Option<Place> {
    Some(match s {
        Slot::Widget {
            widget: Some(w),
            path,
        } => match path[..] {
            // A Scale read follows a bound Size (see `Eval::read`).
            [(_, id)]
                if id == BUILTIN | builtin::DISPLAYED_SIZE || id == BUILTIN | builtin::SCALE =>
            {
                Place::Widget(*w, BUILTIN | builtin::SIZE)
            }
            [(_, id)] => Place::Widget(*w, id),
            [(_, comp), (_, prop)] => Place::Component(*w, comp, prop),
            _ => return None,
        },
        Slot::Template(i, _) => Place::Template(*i),
        _ => return None,
    })
}

/// A slot resolved once per template: what [`Eval`] reads and writes for
/// it, with everything the file itself answers looked up already.
#[derive(Debug, Clone)]
enum Access {
    /// Reads and writes nothing: a clip, a widget path that is too short.
    None,
    Const(Val),
    /// A widget of a nested template (slot index): writes queue a
    /// [`NestedWrite`].
    Nested(usize),
    /// The template's own property; `default` is None when the index names
    /// no property.
    Template {
        place: u32,
        field: u32,
        default: Option<Val>,
    },
    /// A built-in widget property, `id` without [`BUILTIN`].
    Builtin {
        w: usize,
        id: u32,
        field: u32,
    },
    /// A container's template property; on the template's own root, a
    /// property of its WidgetType, which the instance passes in.
    Input {
        place: u32,
        w: usize,
        field: u32,
    },
    /// A component property. Reads take it from `found`, the widget's own
    /// component or a child's (a list reads its item prototype's): what it
    /// stores, else its template's default. A read nothing answers is game
    /// data along path `data`. Axis writes start from `own`, what the
    /// widget's own component stores.
    Component {
        place: u32,
        field: u32,
        prop: u32,
        found: bool,
        stored: Option<Val>,
        template: Option<Guid>,
        own: Option<Val>,
        data: u32,
    },
    /// Game data along path `Prepared::paths[_]`.
    Data(u32),
    /// A field of a container's data-object input (a path of more than one
    /// step that is not a component): read as game data, but a write sets
    /// it here and is passed to the template, whose root reads the same
    /// path (the close button's `-IconData` icon, set from IconReplaceToBack).
    Field {
        w: usize,
        data: u32,
    },
}

/// How a widget slot (widget `w`, `path`) is read and written. `data`
/// names the path for game-data fallbacks.
fn widget_access(
    w: usize,
    path: &[(u32, u32)],
    place: Option<u32>,
    components: &[Vec<super::layout::Component>],
    widgets: &[Widget],
    data: u32,
) -> Access {
    match *path {
        [(field, id)] if id & BUILTIN != 0 => Access::Builtin {
            w,
            id: id & !BUILTIN,
            field,
        },
        [(field, _)] => Access::Input {
            place: place.expect("a one-step widget path has a place"),
            w,
            field,
        },
        [(_, comp), (field, prop)] => {
            let own = components
                .get(w)
                .and_then(|c| c.iter().find(|c| c.uid == comp));
            let found = own.or_else(|| {
                widgets
                    .get(w)?
                    .children
                    .iter()
                    .find_map(|&k| components.get(k)?.iter().find(|c| c.uid == comp))
            });
            Access::Component {
                place: place.expect("a component path has a place"),
                field,
                prop,
                found: found.is_some(),
                stored: found.and_then(|c| c.get(prop)).map(Val::of),
                template: found.and_then(|c| c.template),
                own: own.and_then(|c| c.get(prop)).map(Val::of),
                data,
            }
        }
        [] => Access::None,
        _ if w != 0 => Access::Field { w, data },
        _ => Access::Data(data),
    }
}

impl Prepared {
    /// Whether a binding of this file writes widget `w`'s property `uid`
    /// (for a container, its template input), whole or a field of it.
    pub fn writes_input(&self, w: usize, uid: u32) -> bool {
        self.input_writes.binary_search(&(w, uid)).is_ok()
    }

    /// Whether a binding of this file writes its template property `i`
    /// (an index into [`Asset::properties`]).
    pub fn writes_property(&self, i: usize) -> bool {
        self.place_ids
            .get(&Place::Template(i))
            .is_some_and(|&p| !self.writers[p as usize].is_empty())
    }

    /// Treats `widgets` as stored hidden: binding reads of their Visible see
    /// false until a binding writes it.
    pub fn start_hidden(mut self, widgets: Vec<usize>) -> Prepared {
        for &w in &widgets {
            if let Some(g) = self.geometry.get_mut(w) {
                g.visible = false;
            }
        }
        self.idle_hidden = widgets;
        self
    }

    pub fn new(asset: &Asset, widgets: &[Widget]) -> Prepared {
        let table = asset.bindings(widgets);
        let widget_slot = |s: usize| match table.slots.get(s) {
            Some(Slot::Widget {
                widget: Some(w),
                path,
            }) => match path[..] {
                [(field, id)] => Some((*w, field, id)),
                _ => None,
            },
            _ => None,
        };
        let mut box_readers: Vec<usize> = (0..table.slots.len())
            .filter_map(widget_slot)
            .filter(|&(_, field, id)| id == BUILTIN | builtin::SIZE && (2..=5).contains(&field))
            .map(|(w, ..)| w)
            .chain(
                table
                    .bindings
                    .iter()
                    .flat_map(|b| b.sources.iter().filter_map(|&s| widget_slot(s)))
                    .filter(|&(_, _, id)| {
                        [builtin::SIZE, builtin::DISPLAYED_SIZE, builtin::SCALE]
                            .iter()
                            .any(|b| id == BUILTIN | b)
                    })
                    .map(|(w, ..)| w),
            )
            .collect();
        box_readers.sort_unstable();
        box_readers.dedup();
        let mut output_sources: Vec<usize> = table
            .bindings
            .iter()
            .flat_map(|b| b.sources.iter().filter_map(|&s| widget_slot(s)))
            .filter(|&(_, _, id)| id & BUILTIN == 0)
            .map(|(w, ..)| w)
            .collect();
        // A container's object passed on whole into another reads what the
        // receiver writes into it back (see `Eval::forward`).
        for b in &table.bindings {
            if let [s] = b.sources[..]
                && widget_slot(s).is_some_and(|(.., id)| id & BUILTIN == 0)
            {
                output_sources.extend(
                    b.targets
                        .iter()
                        .filter_map(|&t| widget_slot(t))
                        .map(|(w, ..)| w),
                );
            }
        }
        output_sources.sort_unstable();
        output_sources.dedup();
        let mut keys: Vec<Place> = Vec::new();
        let mut place_ids: super::layout::FastMap<Place, u32> = Default::default();
        let places: Vec<Option<u32>> = table
            .slots
            .iter()
            .map(|s| {
                let place = place_of(s)?;
                Some(*place_ids.entry(place).or_insert_with(|| {
                    keys.push(place);
                    keys.len() as u32 - 1
                }))
            })
            .collect();
        let mut writers: Vec<Vec<u32>> = vec![Vec::new(); keys.len()];
        for (bi, b) in table.bindings.iter().enumerate() {
            for &t in &b.targets {
                if let Some(Some(p)) = places.get(t) {
                    writers[*p as usize].push(bi as u32);
                }
            }
        }
        let mut groups: Vec<Vec<u32>> = Vec::new();
        let mut group_of: Vec<Vec<u32>> = vec![Vec::new(); table.bindings.len()];
        for (p, w) in writers.iter().enumerate() {
            if w.len() < 2 {
                continue;
            }
            let g = groups.len() as u32;
            let touches = |s: &usize| places.get(*s) == Some(&Some(p as u32));
            let members: Vec<u32> = table
                .bindings
                .iter()
                .enumerate()
                .filter(|(_, b)| b.sources.iter().any(touches) || b.targets.iter().any(touches))
                .map(|(i, _)| i as u32)
                .collect();
            for &m in &members {
                group_of[m as usize].push(g);
            }
            groups.push(members);
        }
        let components: Vec<Vec<super::layout::Component>> =
            widgets.iter().map(|w| asset.components(w)).collect();
        let props = asset.properties();
        let mut paths: Vec<Vec<(u32, u32)>> = Vec::new();
        let mut path_owner: Vec<usize> = Vec::new();
        let mut path_id = |owner: usize, path: &[(u32, u32)]| match paths
            .iter()
            .zip(&path_owner)
            .position(|(p, &o)| o == owner && p[..] == *path)
        {
            Some(i) => i as u32,
            None => {
                paths.push(path.to_vec());
                path_owner.push(owner);
                paths.len() as u32 - 1
            }
        };
        let access = table
            .slots
            .iter()
            .zip(&places)
            .enumerate()
            .map(|(i, (s, &place))| match s {
                Slot::Constant { value, .. } => Access::Const(Val::of(value)),
                Slot::Template(t, field) => Access::Template {
                    place: place.expect("a template slot has a place"),
                    field: *field,
                    default: props.get(*t).map(|p| Val::of(&asset.default_at(p.default))),
                },
                Slot::Widget { widget: None, path } => Access::Data(path_id(0, path)),
                Slot::Widget {
                    widget: Some(w),
                    path,
                } => {
                    let data = if path.len() >= 2 {
                        path_id(*w, path)
                    } else {
                        u32::MAX
                    };
                    widget_access(*w, path, place, &components, widgets, data)
                }
                Slot::Nested { .. } => Access::Nested(i),
                Slot::Clip { .. } => Access::None,
            })
            .collect();
        let mut positioned = Vec::new();
        let mut converts = Vec::new();
        for (out, key) in keys.iter().enumerate() {
            let Place::Component(w, uid, POSITIONING_OUTPUT) = *key else {
                continue;
            };
            let is_positioning = components.get(w).is_some_and(|c| {
                c.iter()
                    .any(|c| c.uid == uid && c.template == Some(POSITIONING))
            });
            if !is_positioning {
                continue;
            }
            positioned.push(w);
            if let Some(&input) = place_ids.get(&Place::Component(w, uid, POSITIONING_INPUT)) {
                converts.push((input, out as u32));
            }
        }
        // ValueComponent: MinValue and MaxValue take in MyValue.
        let mut value_outputs = Vec::new();
        for (out, key) in keys.iter().enumerate() {
            let Place::Component(w, uid, prop @ (VALUE_MIN | VALUE_MAX)) = *key else {
                continue;
            };
            // On the widget, or a child (a root path reaches them).
            let comp = components
                .get(w)
                .and_then(|c| c.iter().find(|c| c.uid == uid))
                .or_else(|| {
                    widgets
                        .get(w)?
                        .children
                        .iter()
                        .find_map(|&k| components.get(k)?.iter().find(|c| c.uid == uid))
                })
                .filter(|c| c.template == Some(VALUE_COMPONENT));
            if let Some(c) = comp
                && let Some(&input) = place_ids.get(&Place::Component(w, uid, VALUE_MY))
            {
                converts.push((input, out as u32));
                value_outputs.push((input, out as u32, prop == VALUE_MAX, c.get(prop).cloned()));
            }
        }
        positioned.sort_unstable();
        positioned.dedup();
        let mut arg_at = Vec::with_capacity(table.bindings.len() + 1);
        let mut at = 0u32;
        for b in &table.bindings {
            arg_at.push(at);
            at += b.sources.len().max(1) as u32;
        }
        arg_at.push(at);
        let geometry: Vec<super::Geometry> = widgets
            .iter()
            .map(|w| asset.geometry(w).unwrap_or_default())
            .collect();
        let mut input_writes: Vec<(usize, u32)> = table
            .bindings
            .iter()
            .flat_map(|b| &b.targets)
            .filter_map(|&t| match table.slots.get(t)? {
                Slot::Widget {
                    widget: Some(w),
                    path,
                } => Some((*w, path.first()?.1)),
                _ => None,
            })
            .collect();
        input_writes.sort_unstable();
        input_writes.dedup();
        // Inputs a binding writes whole: what the instance stores under
        // them is the designer's preview (`"This is the description of a
        // quest objective."`, a minimap sector's selected state), which the
        // game's data replaces.
        let whole_writes: Vec<(usize, u32)> = table
            .bindings
            .iter()
            .flat_map(|b| &b.targets)
            .filter_map(|&t| match table.slots.get(t)? {
                Slot::Widget {
                    widget: Some(w),
                    path,
                } if path.len() == 1 => Some((*w, path[0].1)),
                _ => None,
            })
            .collect();
        let stored_fields: Vec<(usize, Vec<(u32, u32)>, DefaultValue)> = widgets
            .iter()
            .enumerate()
            .filter(|(_, widget)| asset.container_template(widget).is_some())
            .flat_map(|(w, widget)| {
                let comps = components.get(w);
                let whole = &whole_writes;
                asset
                    .property_values(widget)
                    .into_iter()
                    .filter(move |(path, _)| {
                        path.len() >= 2
                            && !comps.is_some_and(|c| c.iter().any(|c| c.uid == path[0]))
                            && !whole.contains(&(w, path[0]))
                            // A DataSource the game sets replaces them all.
                            && !whole.contains(&(w, DATA_SOURCE))
                    })
                    .map(move |(path, v)| (w, path.iter().map(|&u| (1, u)).collect(), v))
            })
            .collect();
        Prepared {
            input_writes,
            stored_fields,
            value_outputs,
            idle_hidden: Vec::new(),
            hover_zones: Vec::new(),
            container_defaults: std::sync::OnceLock::new(),
            opening_clips: table.clips_started_on_open(),
            state_clips: table.clips_started_by_state(),
            opening_nested: table.nested_clips_started_on_open(),
            converters: table
                .bindings
                .iter()
                .map(|b| Converter::of(b.converter))
                .collect(),
            styles: geometry
                .iter()
                .map(|g| g.enum_entry)
                .filter(|&e| e != 0)
                .collect(),
            arg_at,
            access,
            paths,
            path_owner,
            groups,
            group_of,
            writers,
            output_sources,
            box_readers,
            positioned,
            converts,
            places,
            keys,
            place_ids,
            table,
            ids: asset.widget_ids(widgets),
            geometry,
            components,
            stored_inputs: widgets
                .iter()
                .map(|w| {
                    let mut m: HashMap<u32, DefaultValue> = asset
                        .property_values(w)
                        .into_iter()
                        .filter_map(|(path, v)| match path[..] {
                            [uid] => Some((uid, v)),
                            _ => None,
                        })
                        .collect();
                    m.entry(CURRENT_ENUM_VALUE)
                        .or_insert(DefaultValue::U32(asset.current_enum_value(w)));
                    m
                })
                .collect(),
            templates: widgets
                .iter()
                .map(|w| asset.container_template(w).map(|r| r.id))
                .collect(),
        }
    }
}
/// What evaluation wrote into one widget.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bound {
    pub position: Option<[f32; 3]>,
    pub pivot: Option<[f32; 3]>,
    pub rotation: Option<f32>,
    pub scale: Option<[f32; 3]>,
    /// Drawn size in pixels; the layout turns it into Scale.
    pub size: Option<[f32; 2]>,
    pub alpha: Option<f32>,
    pub visible: Option<bool>,
    /// ColorTint RGB.
    pub tint: Option<[u8; 3]>,
    /// UVOffset / UVScale per axis; an axis no binding wrote keeps the
    /// stored value.
    pub uv_offset: [Option<f32>; 2],
    pub uv_scale: [Option<f32>; 2],
    /// A label's TextPlane per axis.
    pub text_plane: [Option<f32>; 2],
    /// A label's sizing, alignment and overflow enums per axis.
    pub sizing: [Option<u32>; 2],
    pub alignment: [Option<u32>; 2],
    pub overflow: [Option<u32>; 2],
    /// Label LineSpacing.
    pub line_spacing: Option<f32>,
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
            && *class == super::CLASS_COLOR_TYPE
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
                .and_then(|(g, e)| super::widget::palette_color(g, e))
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
            self.values_of(root, PROP_CONVERSION_RULES)
                .into_iter()
                .filter_map(|r| {
                    let values = self
                        .values_of(r, PROP_CONVERSION_VALUES)
                        .into_iter()
                        .map(|v| {
                            let value = self
                                .values_of(v, PROP_VALUE)
                                .into_iter()
                                .find(|&s| matches!(self.spans[s].record, Record::Default(_)))
                                .map(|s| self.default_at(s));
                            (self.u32_of(v, PROP_CONVERSION_ENTRY), value)
                        })
                        .collect::<Vec<_>>();
                    Some((self.u32_of(r, PROP_RULE_ID)?, values))
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
            .values_of(root, PROP_CONSTANTS)
            .into_iter()
            .find(|&c| self.values_of(c, PROP_DEFAULT).contains(&default))?;
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
        let target = |step: usize| match self.values_of(step, PROP_TARGET_WIDGET).first() {
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
            .values_of(root, PROP_PROVIDERS)
            .into_iter()
            .map(|p| {
                let steps = self.values_of(p, PROP_STEPS);
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
        for c in self.values_of(root, PROP_CONTAINERS) {
            let path = self
                .values_of(c, PROP_PATH)
                .into_iter()
                .map(|id| {
                    (
                        self.u32_of(id, PROP_FIELD).unwrap_or(1),
                        self.u32_of(id, PROP_UNIQUE_ID).unwrap_or(0),
                    )
                })
                .collect();
            let provider = self
                .u32_of(c, PROP_PROVIDER)
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
        for c in self.values_of(root, PROP_TEMPLATE_CONTAINERS) {
            slots.push(Slot::Template(
                self.u32_of(c, PROP_TEMPLATE_INDEX).unwrap_or(u32::MAX) as usize,
                self.u32_of(c, PROP_FIELD).unwrap_or(1),
            ));
        }
        let mut constants = HashMap::new();
        for c in self.values_of(root, PROP_CONSTANTS) {
            let span = self
                .values_of(c, PROP_DEFAULT)
                .into_iter()
                .find(|&s| matches!(self.spans[s].record, Record::Default(_)));
            if let Some(s) = span {
                constants.insert(slots.len(), s);
            }
            slots.push(Slot::Constant {
                kind: self.u32_of(c, PROP_PROPERTY_TYPE),
                value: span.map_or(DefaultValue::None, |s| self.default_at(s)),
            });
        }
        let index = |s: usize| match self.spans[s].record {
            Record::Value(Value::U32(v)) => Some(v as usize),
            _ => None,
        };
        let bindings =
            self.values_of(root, PROP_BINDINGS)
                .into_iter()
                .map(|b| {
                    let conv = self.values_of(b, PROP_CONVERTER).first().copied();
                    let mode = conv
                        .and_then(|m| self.u32_of(m, PROP_BINDING_MODE))
                        .unwrap_or(0);
                    let converter = conv.map_or(0, |m| match &self.spans[m].record {
                        Record::Object(h) => h.class,
                        _ => 0,
                    });
                    let rules = conv
                        .map(|m| {
                            self.values_of(m, PROP_BINDING_INFOS)
                                .into_iter()
                                .filter_map(|i| {
                                    let entry = self.u32_of(i, PROP_RULE_ENTRY)?;
                                    let target = self
                                        .values_of(i, PROP_RULE_TARGET)
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
                        entry: self.u32_of(b, super::widget::PROP_ENUM_ENTRY).unwrap_or(0),
                        enum_rule: conv.and_then(|m| {
                            let asset = self.values_of(m, PROP_ENUM_ASSET).iter().find_map(
                                |&s| match &self.spans[s].record {
                                    Record::Ref(r) => Some(r.id),
                                    _ => None,
                                },
                            )?;
                            Some((asset, self.u32_of(m, PROP_RULE_ID)?))
                        }),
                        converter,
                        sources: self
                            .values_of(b, PROP_SOURCES)
                            .into_iter()
                            .filter_map(index)
                            .collect(),
                        targets: self
                            .values_of(b, PROP_TARGETS)
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

const CLASS_CONTAINER: u32 = name_crc(b"PropertyContainer");
const CLASS_PROPERTY_ID: u32 = name_crc(b"PropertyId");
const CLASS_PROVIDER: u32 = name_crc(b"DataProviderContainer");
const CLASS_STEP: u32 = name_crc(b"DataProviderId");
const CLASS_TEMPLATE_CONTAINER: u32 = name_crc(b"CustomPropertyContainer");
const CLASS_CONSTANT: u32 = name_crc(b"ConstantPropertyContainer");
const CLASS_BINDING: u32 = 0xedcf_0433;
/// The class widget references in provider steps name.
const CLASS_WIDGET_REF: u32 = 0xbb96_607d;
const PROP_USE_COUNT: u32 = name_crc(b"UseCount");
const PROP_PROFILE_ID: u32 = name_crc(b"ProfileID");
/// Timeline tracks name their slot in the same table.
const PROP_TRACK_TARGET: u32 = 0x1935_a21e;

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

fn object_header(out: &mut Vec<u8>, class: u32, props: u8, mask: &[u8]) {
    out.extend_from_slice(&[0x02, 0x00]);
    out.extend_from_slice(&class.to_le_bytes());
    out.push(props);
    out.extend_from_slice(mask);
}

fn u32s(out: &mut Vec<u8>, values: &[u32]) {
    super::write_varint(out, values.len() as u32);
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
pub(super) struct SlotPlan<'a> {
    asset: &'a Asset,
    widgets: &'a [Widget],
    table: Bindings,
    /// Containers, template containers, constants, providers, bindings.
    pub(super) arrays: [(usize, Vec<usize>); 5],
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
    pub(super) fn new(asset: &'a Asset, widgets: &'a [Widget]) -> anyhow::Result<SlotPlan<'a>> {
        let root = widgets
            .first()
            .map(|w| w.object)
            .ok_or_else(|| anyhow::anyhow!("no widgets"))?;
        let arrays = asset.binding_arrays(root)?;
        let steps = arrays[3]
            .1
            .iter()
            .map(|&p| {
                asset
                    .values_of(p, PROP_STEPS)
                    .into_iter()
                    .map(|step| {
                        let mode = match asset.values_of(step, PROP_DATA_MODE).first() {
                            Some(&m) => match asset.spans[m].record {
                                Record::Value(Value::Enum { value, .. } | Value::U32(value)) => {
                                    value
                                }
                                _ => return None,
                            },
                            None => 0,
                        };
                        let target = match asset.values_of(step, PROP_TARGET_WIDGET).first() {
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
            arrays,
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
        let (np, nt) = (self.arrays[0].1.len(), self.arrays[1].1.len());
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
                    *self.uses.entry(self.arrays[0].1[i]).or_default() += 1;
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
                    *self.uses.entry(self.arrays[0].1[i]).or_default() += 1;
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
                    *self.uses.entry(self.arrays[0].1[i]).or_default() += 1;
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
                    *self.uses.entry(self.arrays[1].1[k - np]).or_default() += 1;
                    return Ok(At::T(k - np));
                }
                let mut c = Vec::new();
                object_header(&mut c, CLASS_TEMPLATE_CONTAINER, 4, &[0x70]);
                c.extend_from_slice(&(*i as u32).to_le_bytes());
                c.extend_from_slice(&field.to_le_bytes());
                c.extend_from_slice(&1u32.to_le_bytes());
                self.new_t.push(c);
                At::T(nt + self.new_t.len() - 1)
            }
            NewSlot::Copied(bytes) => {
                self.new_c.push(bytes.clone());
                At::C(self.arrays[2].1.len() + self.new_c.len() - 1)
            }
            NewSlot::Constant { kind, value } => {
                let mut c = Vec::new();
                object_header(&mut c, CLASS_CONSTANT, 4, &[0x70]);
                c.extend_from_slice(&kind.to_le_bytes());
                c.extend_from_slice(&[0; 16]);
                value.encode(&mut c);
                self.new_c.push(c);
                At::C(self.arrays[2].1.len() + self.new_c.len() - 1)
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
                Some(k) => self.arrays[3].1.len() + k,
                None => {
                    self.new_providers.push(steps);
                    self.arrays[3].1.len() + self.new_providers.len() - 1
                }
            },
        };
        let mut c = Vec::new();
        object_header(&mut c, CLASS_CONTAINER, 4, &[0x70]);
        super::write_varint(&mut c, path.len() as u32);
        for &(field, uid) in path {
            object_header(&mut c, CLASS_PROPERTY_ID, 3, &[0x60]);
            c.extend_from_slice(&field.to_le_bytes());
            c.extend_from_slice(&uid.to_le_bytes());
        }
        c.extend_from_slice(&(provider as u32).to_le_bytes());
        c.extend_from_slice(&1u32.to_le_bytes());
        self.new_p.push(c);
        At::P(self.arrays[0].1.len() + self.new_p.len() - 1)
    }

    /// One more use of existing slot `v` (a binding copy sharing it): a
    /// container's or template container's UseCount goes up; a constant is
    /// copied instead, one per binding as the game's files have them.
    pub(super) fn share(&mut self, v: u32) -> Option<At> {
        let (np, nt) = (self.arrays[0].1.len(), self.arrays[1].1.len());
        let v = v as usize;
        if v < np {
            *self.uses.entry(self.arrays[0].1[v]).or_default() += 1;
            None
        } else if v < np + nt {
            *self.uses.entry(self.arrays[1].1[v - np]).or_default() += 1;
            None
        } else {
            let c = *self.arrays[2].1.get(v - np - nt)?;
            self.new_c
                .push(self.asset.bytes[self.asset.subtree_bytes(c)].to_vec());
            Some(At::C(self.arrays[2].1.len() + self.new_c.len() - 1))
        }
    }

    /// Where stored slot index `v` goes once the planned slots are in:
    /// new containers go after the existing ones, new template containers
    /// after theirs, pushing the later regions along.
    pub(super) fn renumber(&self, v: u32) -> u32 {
        let (np, nt) = (self.arrays[0].1.len() as u32, self.arrays[1].1.len() as u32);
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
        let (np, nt) = (self.arrays[0].1.len() as u32, self.arrays[1].1.len() as u32);
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
                    Some(PROP_SOURCES | PROP_TARGETS) => h.class == CLASS_BINDING,
                    Some(PROP_TRACK_TARGET) => true,
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
            let Some(&span) = asset.values_of(*object, PROP_USE_COUNT).first() else {
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
                object_header(&mut p, CLASS_PROVIDER, 2, &[0x40]);
                super::write_varint(&mut p, steps.len() as u32);
                for &(mode, target) in steps {
                    object_header(&mut p, CLASS_STEP, 3, &[0x60]);
                    p.extend_from_slice(&mode.to_le_bytes());
                    match target {
                        None => p.push(0),
                        Some(id) => {
                            p.push(1);
                            p.extend_from_slice(&CLASS_WIDGET_REF.to_le_bytes());
                            p.extend_from_slice(&id.0);
                        }
                    }
                }
                p
            })
            .collect();
        let [containers, templates, constants, provider_array, _] = &self.arrays;
        append(asset, &mut splices, &mut counts, containers, self.new_p);
        append(asset, &mut splices, &mut counts, templates, self.new_t);
        append(asset, &mut splices, &mut counts, constants, self.new_c);
        append(asset, &mut splices, &mut counts, provider_array, providers);
        (splices, counts)
    }
}

/// The classes a [`SlotPlan`] may add.
pub(super) const SLOT_CLASSES: [u32; 6] = [
    CLASS_CONTAINER,
    CLASS_PROPERTY_ID,
    CLASS_PROVIDER,
    CLASS_STEP,
    CLASS_TEMPLATE_CONTAINER,
    CLASS_CONSTANT,
];

impl Asset {
    /// The root's five binding tables: (count span, elements) each, in
    /// slot order, then the providers and the bindings.
    fn binding_arrays(&self, root: usize) -> anyhow::Result<[(usize, Vec<usize>); 5]> {
        let get = |crc: u32, what: &str| {
            self.array(root, crc)
                .ok_or_else(|| anyhow::anyhow!("this file stores no {what} table"))
        };
        Ok([
            get(PROP_CONTAINERS, "property container")?,
            get(PROP_TEMPLATE_CONTAINERS, "template property container")?,
            get(PROP_CONSTANTS, "constant")?,
            get(PROP_PROVIDERS, "provider")?,
            get(PROP_BINDINGS, "binding")?,
        ])
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
        object_header(&mut binding, CLASS_BINDING, 9, &[0x7f, 0x80]);
        object_header(&mut binding, b.converter, 5, &[0x78]);
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
        let bindings = plan.arrays[4].clone();
        let (mut splices, mut counts) = plan.finish();
        append(self, &mut splices, &mut counts, &bindings, vec![binding]);
        self.finish(
            splices,
            counts,
            &[
                CLASS_CONTAINER,
                CLASS_PROPERTY_ID,
                CLASS_PROVIDER,
                CLASS_STEP,
                CLASS_TEMPLATE_CONTAINER,
                CLASS_CONSTANT,
                CLASS_BINDING,
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
        let bindings = plan.arrays[4].clone();
        // Every slot planned before any is numbered: numbers move as the
        // regions before them grow.
        let mut planned = Vec::with_capacity(copies.len());
        for c in copies {
            let &obj = bindings
                .1
                .get(c.binding)
                .ok_or_else(|| anyhow::anyhow!("no binding {}", c.binding))?;
            let values: Vec<usize> = self
                .values_of(obj, PROP_SOURCES)
                .into_iter()
                .chain(self.values_of(obj, PROP_TARGETS))
                .collect();
            let sources = self.values_of(obj, PROP_SOURCES).len();
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
            if let Some(&id) = self.values_of(obj, PROP_PROFILE_ID).first() {
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
        self.finish(splices, counts, &SLOT_CLASSES)
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
                    && uid & BUILTIN == 0
                    && let Some(r) = widgets.get(*w).and_then(|wd| self.container_template(wd))
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
        let &object = plan.arrays[4]
            .1
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("no binding {index}"))?;
        let &span = self
            .values_of(object, PROP_SOURCES)
            .get(k)
            .ok_or_else(|| anyhow::anyhow!("binding {index} has no source {k}"))?;
        let Record::Value(Value::U32(old)) = self.spans[span].record else {
            anyhow::bail!("binding {index}'s source {k} is not an index");
        };
        let (containers, templates) = (plan.arrays[0].1.clone(), plan.arrays[1].1.clone());
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
            && let Some(&count) = self.values_of(c, PROP_USE_COUNT).first()
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
        self.finish(splices, counts, &SLOT_CLASSES)
    }

    pub fn remove_binding(&self, widgets: &[Widget], index: usize) -> anyhow::Result<Asset> {
        let root = widgets
            .first()
            .map(|w| w.object)
            .ok_or_else(|| anyhow::anyhow!("no widgets"))?;
        let [containers, templates, _, _, bindings] = self.binding_arrays(root)?;
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
            if let Some(&span) = self.values_of(c, PROP_USE_COUNT).first()
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

/// A value as evaluation holds it: a [`DefaultValue`] with vectors inline
/// and strings shared, so reading and writing slots does not allocate.
/// Unused vector lanes stay zero, which keeps equality the
/// [`DefaultValue`]'s.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    None,
    Bool(u8),
    S32(i32),
    U32(u32),
    F32(u32),
    /// Two to four f32 bits (`len` of them).
    Vec {
        len: u8,
        bits: [u32; 4],
    },
    Str(Arc<str>),
    ObjectId(Guid),
    Object(u32),
}

impl Val {
    fn of(v: &DefaultValue) -> Val {
        match v {
            DefaultValue::None => Val::None,
            DefaultValue::Bool(b) => Val::Bool(*b),
            DefaultValue::S32(i) => Val::S32(*i),
            DefaultValue::U32(u) => Val::U32(*u),
            DefaultValue::F32(f) => Val::F32(*f),
            DefaultValue::Vec(x) => {
                // The wire has Vector2/3/4 only; every vector built here
                // is at most as wide as its widest operand.
                assert!(x.len() <= 4, "a {}-wide vector value", x.len());
                let mut bits = [0; 4];
                bits[..x.len()].copy_from_slice(x);
                Val::Vec {
                    len: x.len() as u8,
                    bits,
                }
            }
            DefaultValue::Str(s) => Val::Str(s.as_str().into()),
            DefaultValue::ObjectId(g) => Val::ObjectId(*g),
            DefaultValue::Object(c) => Val::Object(*c),
        }
    }

    fn dv(&self) -> DefaultValue {
        match self {
            Val::None => DefaultValue::None,
            Val::Bool(b) => DefaultValue::Bool(*b),
            Val::S32(i) => DefaultValue::S32(*i),
            Val::U32(u) => DefaultValue::U32(*u),
            Val::F32(f) => DefaultValue::F32(*f),
            Val::Vec { len, bits } => DefaultValue::Vec(bits[..*len as usize].to_vec()),
            Val::Str(s) => DefaultValue::Str(s.to_string()),
            Val::ObjectId(g) => DefaultValue::ObjectId(*g),
            Val::Object(c) => DefaultValue::Object(*c),
        }
    }

    /// A vector of `f` (at most four).
    fn floats(f: &[f32]) -> Val {
        let mut bits = [0; 4];
        for (b, x) in bits.iter_mut().zip(f) {
            *b = x.to_bits();
        }
        Val::Vec {
            len: f.len() as u8,
            bits,
        }
    }

    fn f32(x: f32) -> Val {
        Val::F32(x.to_bits())
    }
}

/// A value's numbers: a scalar is one, a vector its lanes.
#[derive(Debug, Clone, Copy, Default)]
struct Floats {
    len: usize,
    f: [f32; 4],
}

impl std::ops::Deref for Floats {
    type Target = [f32];
    fn deref(&self) -> &[f32] {
        &self.f[..self.len]
    }
}

impl std::ops::DerefMut for Floats {
    fn deref_mut(&mut self) -> &mut [f32] {
        &mut self.f[..self.len]
    }
}

/// One axis of a vector read through PropertyField `field` (2 x, 3 y,
/// 4 z, 5 w); a missing axis reads nothing.
fn pick(f: &[f32], field: u32) -> Option<Val> {
    f.get(field as usize - 2).map(|x| Val::f32(*x))
}

/// `whole` as read through PropertyField `field`: one axis of a vector
/// for 2–5, else all of it.
fn axis(whole: Val, field: u32) -> Option<Val> {
    match (field, floats(&whole)) {
        (2..=5, Some(f)) if f.len() > 1 => pick(&f, field),
        _ => Some(whole),
    }
}

/// `base` (a vector, or nothing yet) with axis `field` (2 x … 5 w) set from
/// `value`: a scalar, or a vector whose matching component is taken.
fn set_axis(base: Option<Val>, field: u32, value: &Val) -> Val {
    let k = field as usize - 2;
    let Some(v) = floats(value).and_then(|f| f.get(if f.len() > 1 { k } else { 0 }).copied())
    else {
        return value.clone();
    };
    let mut out = base.as_ref().and_then(floats).unwrap_or_default();
    // Lanes past `len` are zero (see `floats`), so growing pads with 0.
    out.len = out.len.max((k + 1).max(2));
    out[k] = v;
    Val::floats(&out)
}

fn floats(v: &Val) -> Option<Floats> {
    let one = |x: f32| Floats {
        len: 1,
        f: [x, 0., 0., 0.],
    };
    match v {
        Val::F32(b) => Some(one(f32::from_bits(*b))),
        Val::Vec { len, bits } => Some(Floats {
            len: *len as usize,
            f: bits.map(f32::from_bits),
        }),
        Val::S32(i) => Some(one(*i as f32)),
        Val::U32(u) => Some(one(*u as f32)),
        _ => None,
    }
}

fn vec3(v: &Val, keep_z: f32) -> Option<[f32; 3]> {
    let f = floats(v)?;
    match f[..] {
        [x, y, z] => Some([x, y, z]),
        [x, y] => Some([x, y, keep_z]),
        _ => None,
    }
}

/// TextInput WidgetType (`c9dfb3c3…`) properties: game code shows `Text`
/// as `DisplayedText` (masked in PasswordMode), which the template binds to
/// its label.
const TEXT_INPUT_TEXT: u32 = 0x30e6_0884;
const TEXT_INPUT_DISPLAYED_TEXT: u32 = 0x3203_36b3;

/// `CurrentEnumValue`: the template property of an enum widget that picks
/// which `EnumEntry`-tagged widgets and bindings are live.
pub const CURRENT_ENUM_VALUE: u32 = 0x3000_0004;

/// The live style of an enum widget template: what the instance passes into
/// `CurrentEnumValue` (its stored field, or a parent binding). An instance
/// always passes one; a value that names no entry, 0 included, shows none of
/// the tagged widgets, as in the game. A template opened on its own shows
/// its first tagged widget's style.
pub fn current_style(
    asset: &Asset,
    widgets: &[Widget],
    inputs: &HashMap<u32, DefaultValue>,
) -> u32 {
    match inputs.get(&CURRENT_ENUM_VALUE) {
        Some(DefaultValue::U32(v)) => *v,
        Some(DefaultValue::S32(v)) => *v as u32,
        Some(_) => 0,
        None => widgets
            .iter()
            .filter_map(|w| asset.geometry(w))
            .map(|g| g.enum_entry)
            .find(|&e| e != 0)
            .unwrap_or(0),
    }
}

fn to_bits(v: [f32; 3]) -> Val {
    Val::floats(&v)
}

/// Rounds of a file's bindings before giving up on a fixpoint.
const MAX_PASSES: usize = 10;

/// What the surrounding tree feeds one file's evaluation.
#[derive(Clone, Copy)]
pub struct Feed<'a> {
    /// The template property values a container instance passes in
    /// (`PropertyValues` paths of one id, and its bindings' writes).
    pub inputs: &'a HashMap<u32, DefaultValue>,
    /// Laid-out drawn sizes for widgets whose Size nothing binds.
    pub sizes: &'a [Option<[f32; 2]>],
    /// Per widget: its laid-out transform from its own space to the root
    /// (`layout::Placed::transform`), for [`Prepared::positioned`].
    pub transforms: &'a [Option<[f32; 6]>],
    /// The container instance's geometry, as its own file stores and binds
    /// it: the instance replaces the template root, so `root · Position`,
    /// `RotationAngle`, … read it (the archive column's tabs stand rotated
    /// 90° and turn their icons back by `Sign(root · RotationAngle)`).
    pub root_geometry: Option<&'a super::Geometry>,
    /// Slot values a playing clip or a held interaction state writes
    /// (`animation::Clip::sample`); they win over the bindings.
    pub animated: &'a [(usize, DefaultValue)],
    /// Slot values of opening clips at their end: the screen as opened,
    /// after which the game keeps running the bindings, so they win.
    pub settled: &'a [(usize, DefaultValue)],
    /// What a parent template's bindings write into this file's widgets.
    pub inherited: &'a [NestedWrite],
    /// Sample data is on: unresolved game-data enums take a first entry.
    pub sampling: bool,
    /// Values native code writes into the root's components, by component
    /// template guid and property (a radial list item's slice).
    pub root_components: &'a [(Guid, u32, DefaultValue)],
    /// Values the container instance holds in its components, by component
    /// UniqueId and property: what it stores and what the parent's bindings
    /// write there. The template root reads them as its own components
    /// (the minimap sector writes its animator's AngleSize this way).
    pub instance_components: &'a [(u32, u32, DefaultValue)],
    /// What evaluation reads from the file itself, when the caller keeps
    /// it (a template expands hundreds of times); else read here.
    pub prepared: Option<&'a Prepared>,
    /// Per widget: a container instance's template property values from
    /// the previous build ([`Evaluated::template`]): outputs its template
    /// computes, which this file reads before that template is evaluated.
    pub outputs: &'a [Option<&'a HashMap<u32, DefaultValue>>],
    /// What the caller already hashed: `inputs` (as
    /// [`super::layout::values_hash`]) and `settled` (as a slice).
    pub inputs_hash: Option<u64>,
    pub settled_hash: Option<u64>,
}

impl Feed<'_> {
    /// A hash of everything [`evaluate`] reads from its caller. Equal feeds
    /// of one file evaluate alike (its defaults, enum rules and sample data
    /// are fixed), which lets a layout pass skip instances nothing changed.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let map = super::layout::values_hash;
        let mut h = super::layout::hasher();
        self.inputs_hash
            .unwrap_or_else(|| map(self.inputs))
            .hash(&mut h);
        // Only what evaluation reads: most templates never read a size, so
        // a pass that only learns sizes leaves them alone.
        let all: Vec<usize>;
        let (sizes, outputs) = match self.prepared {
            Some(p) => (&p.box_readers[..], &p.output_sources[..]),
            None => {
                all = (0..self.sizes.len().max(self.outputs.len())).collect();
                (&all[..], &all[..])
            }
        };
        for &w in sizes {
            let s = self.sizes.get(w).copied().flatten();
            s.map(|v| v.map(f32::to_bits)).hash(&mut h);
        }
        self.root_geometry.map(geometry_hash).hash(&mut h);
        for &w in self.prepared.map_or(&[][..], |p| &p.positioned[..]) {
            let t = self.transforms.get(w).copied().flatten();
            t.map(|v| v.map(f32::to_bits)).hash(&mut h);
        }
        self.animated.hash(&mut h);
        match self.settled_hash {
            Some(x) => x.hash(&mut h),
            None => {
                let mut s = super::layout::hasher();
                self.settled.hash(&mut s);
                s.finish().hash(&mut h);
            }
        }
        self.inherited.hash(&mut h);
        self.sampling.hash(&mut h);
        self.root_components.hash(&mut h);
        self.instance_components.hash(&mut h);
        for &w in outputs {
            self.outputs.get(w).copied().flatten().map(map).hash(&mut h);
        }
        h.finish()
    }
}

/// A geometry's hash, floats by their bits.
pub(super) fn geometry_hash(g: &super::Geometry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = super::layout::hasher();
    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    bits(&g.position).hash(&mut h);
    bits(&g.pivot).hash(&mut h);
    g.rotation.to_bits().hash(&mut h);
    bits(&g.scale).hash(&mut h);
    g.alpha.to_bits().hash(&mut h);
    (g.tint, g.visible, g.hidden, g.anchor, g.anchor_metrics).hash(&mut h);
    (
        g.resize_metrics,
        g.aspect_ratio,
        g.z_index,
        g.consider_size,
        g.enum_entry,
    )
        .hash(&mut h);
    h.finish()
}

thread_local! {
    static FULL_SWEEPS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes [`evaluate`] run every binding on every sweep, as before the
/// worklist, on this thread: tests compare the two.
#[doc(hidden)]
pub fn set_full_sweeps(on: bool) {
    FULL_SWEEPS.with(|c| c.set(on));
}

/// See `Prepared::container_defaults`.
fn container_defaults(
    prepared: &Prepared,
    component_default: &mut dyn FnMut(Guid, u32) -> Option<DefaultValue>,
) -> ContainerDefaults {
    let table = &prepared.table;
    let stored = |w: usize, uid: u32| prepared.stored_inputs.get(w)?.get(&uid).cloned();
    let mut container_inputs: HashMap<(usize, u32), DefaultValue> = HashMap::new();
    for b in &table.bindings {
        for &src in &b.sources {
            if let Some(Slot::Widget {
                widget: Some(w),
                path,
            }) = table.slots.get(src)
                && let [(_, uid)] = path[..]
                && uid & BUILTIN == 0
                && *w != 0
                && !container_inputs.contains_key(&(*w, uid))
                && let Some(t) = prepared.templates.get(*w).copied().flatten()
                && let Some(v) = stored(*w, uid).or_else(|| component_default(t, uid))
            {
                container_inputs.insert((*w, uid), v);
            }
        }
    }
    // Container inputs that bindings or clips write one axis of: what the
    // instance stores (else the template's default), which axis writes
    // start from.
    let mut axis_defaults: HashMap<(usize, u32), DefaultValue> = HashMap::new();
    // Every slot, not only binding and clip targets: the same for every
    // instance, whatever clip it plays.
    for t in 0..table.slots.len() {
        if let Some(Slot::Widget {
            widget: Some(w),
            path,
        }) = table.slots.get(t)
            && let [(2..=5, uid)] = path[..]
            && uid & BUILTIN == 0
            && !axis_defaults.contains_key(&(*w, uid))
            && let Some(d) = container_inputs.get(&(*w, uid)).cloned().or_else(|| {
                let t = prepared.templates.get(*w).copied().flatten()?;
                stored(*w, uid).or_else(|| component_default(t, uid))
            })
        {
            axis_defaults.insert((*w, uid), d);
        }
    }
    let per_place = |m: &HashMap<(usize, u32), DefaultValue>| {
        prepared
            .keys
            .iter()
            .map(|k| match k {
                Place::Widget(w, uid) => m.get(&(*w, *uid)).map(Val::of),
                _ => None,
            })
            .collect()
    };
    (per_place(&container_inputs), per_place(&axis_defaults))
}

/// Stands in for a game-object field a data path reads (called with the
/// whole path) when nothing in the tree supplies it.
pub type GameData<'a> = dyn FnMut(&[(u32, u32)]) -> Option<DefaultValue> + 'a;

/// One evaluation's state: the widgets' bound geometry and text, and per
/// place what bindings wrote there so far.
struct Eval<'a> {
    p: &'a Prepared,
    sizes: &'a [Option<[f32; 2]>],
    transforms: &'a [Option<[f32; 6]>],
    root_geometry: Option<&'a super::Geometry>,
    bound: Vec<Bound>,
    text: Vec<Option<Arc<str>>>,
    loc: Vec<Option<Arc<str>>>,
    /// Per place: the value written (inputs, components, template
    /// properties; built-ins live in `bound`). Places past the prepared
    /// ones are in `extra`: what a parent or native code writes that no
    /// slot of this file names.
    reg: Vec<Option<Val>>,
    extra: Vec<Place>,
    /// Per place, looked up on first read: what the caller passes in (a
    /// template property, an input of the root), and a container's outputs
    /// from the last build.
    inputs: &'a HashMap<u32, DefaultValue>,
    outputs: &'a [Option<&'a HashMap<u32, DefaultValue>>],
    props: &'a [super::CustomProperty],
    fed: Vec<Option<Option<Val>>>,
    out: Vec<Option<Option<Val>>>,
    /// Per place: see `Prepared::container_defaults`.
    read_default: &'a [Option<Val>],
    axis_default: &'a [Option<Val>],
    /// Per place: a component template's default, asked for once.
    component_default: Vec<Option<Option<Val>>>,
    /// Per `Prepared::paths` entry: game data, asked for once. Resolving a
    /// path builds its name and runs the sampler; bindings read it again
    /// every sweep.
    data: Vec<Option<Option<Val>>>,
    nested: Vec<Vec<NestedWrite>>,
    /// Fields of the root's data-object inputs a parent wrote or the
    /// instance stores, whether or not this file reads them: an object
    /// input passed on whole carries them along.
    root_fields: Vec<(Vec<(u32, u32)>, DefaultValue)>,
    /// Per widget: how many of its `nested` writes come first and stay
    /// first, the instance's stored fields and then forwarded objects
    /// (see `forward`), so a binding's own field write overrides them
    /// whatever order evaluation runs in.
    nested_base: Vec<usize>,
}

impl Eval<'_> {
    fn key(&self, place: usize) -> Place {
        match self.p.keys.get(place) {
            Some(k) => *k,
            None => self.extra[place - self.p.keys.len()],
        }
    }

    /// The place of `key`, made for this evaluation if no slot names it.
    fn place(&mut self, key: Place) -> u32 {
        if let Some(&p) = self.p.place_ids.get(&key) {
            return p;
        }
        let n = self.p.keys.len();
        if let Some(i) = self.extra.iter().position(|k| *k == key) {
            return (n + i) as u32;
        }
        self.extra.push(key);
        self.reg.push(None);
        (n + self.extra.len() - 1) as u32
    }

    /// What the caller passes into place `p`.
    fn fed(&mut self, p: usize) -> Option<Val> {
        if let Some(v) = &self.fed[p] {
            return v.clone();
        }
        let inputs = self.inputs;
        let v = match self.p.keys[p] {
            Place::Template(i) => self.props.get(i).and_then(|q| inputs.get(&q.uid)),
            Place::Widget(0, uid) if uid & BUILTIN == 0 => inputs.get(&uid).or_else(|| {
                // Native: a text input displays its Text.
                (uid == TEXT_INPUT_DISPLAYED_TEXT)
                    .then(|| inputs.get(&TEXT_INPUT_TEXT))
                    .flatten()
            }),
            _ => None,
        }
        .map(Val::of);
        self.fed[p] = Some(v.clone());
        v
    }

    /// What a container's template computed for place `p` last build.
    fn out(&mut self, p: usize) -> Option<Val> {
        if let Some(v) = &self.out[p] {
            return v.clone();
        }
        let v = match self.p.keys[p] {
            Place::Widget(w, uid) if uid & BUILTIN == 0 => self
                .outputs
                .get(w)
                .copied()
                .flatten()
                .and_then(|o| o.get(&uid))
                .map(Val::of),
            _ => None,
        };
        self.out[p] = Some(v.clone());
        v
    }

    /// A template property as written so far, else what the instance
    /// passes, else its default.
    fn template_value(&mut self, place: u32, default: &Option<Val>) -> Option<Val> {
        let p = place as usize;
        if let Some(v) = &self.reg[p] {
            return Some(v.clone());
        }
        self.fed(p).or_else(|| default.clone())
    }

    /// What slot access `a` reads. A path into game data (a provider, or
    /// two or more steps that are not a component property here) ends in a
    /// game-object field.
    fn read(
        &mut self,
        a: &Access,
        component_default: &mut dyn FnMut(Guid, u32) -> Option<DefaultValue>,
        game_data: &mut GameData,
    ) -> Option<Val> {
        let v = match a {
            Access::None | Access::Nested(_) | Access::Data(_) | Access::Field { .. } => None,
            Access::Const(v) => Some(v.clone()),
            Access::Template {
                place,
                field,
                default,
                ..
            } => self
                .template_value(*place, default)
                .and_then(|whole| axis(whole, *field)),
            Access::Builtin { w, id, field } => self.builtin(*w, *id).and_then(|v| axis(v, *field)),
            Access::Input { place, w, field } => {
                let p = *place as usize;
                let known = match &self.reg[p] {
                    Some(v) => Some(v.clone()),
                    None => self
                        .out(p)
                        .or_else(|| self.read_default.get(p).cloned().flatten()),
                };
                match (field, known) {
                    // A container's template property, as written so far.
                    (2..=5, Some(v)) => match floats(&v) {
                        Some(f) if f.len() > 1 => pick(&f, *field),
                        _ => Some(v),
                    },
                    (_, Some(v)) => Some(v),
                    // On the template's own root it is what the instance
                    // passes in, read whole.
                    (_, None) if *w == 0 => self.fed(p),
                    _ => None,
                }
            }
            Access::Component {
                place,
                field,
                prop,
                found,
                stored,
                template,
                ..
            } => {
                let p = *place as usize;
                if *prop == POSITIONING_OUTPUT
                    && *template == Some(POSITIONING)
                    && let Some(v) = self.converted(p)
                {
                    return axis(v, *field);
                }
                // PropertyField 2–5 read one axis, of a written value too.
                if let Some(v) = &self.reg[p] {
                    axis(v.clone(), *field)
                } else if !found {
                    None
                } else {
                    match stored {
                        Some(v) => Some(v.clone()),
                        None => template.and_then(|t| {
                            self.component_default[p]
                                .get_or_insert_with(|| {
                                    component_default(t, *prop).map(|v| Val::of(&v))
                                })
                                .clone()
                        }),
                    }
                    .and_then(|whole| axis(whole, *field))
                }
            }
        };
        if v.is_some() {
            return v;
        }
        match *a {
            Access::Data(d) | Access::Field { data: d, .. } | Access::Component { data: d, .. } => {
                let d = d as usize;
                if let Some(v) = &self.data[d] {
                    return v.clone();
                }
                let v = game_data(&self.p.paths[d]).map(|v| Val::of(&v));
                self.data[d] = Some(v.clone());
                v
            }
            _ => None,
        }
    }

    /// A Positioning component's Output at place `p`: its Input (as
    /// written, else stored, else 0) by its widget's laid-out transform,
    /// own space to root for LocalToGlobal, back for GlobalToLocal; as it
    /// is for Invalid. None before the widget is laid out.
    fn converted(&self, p: usize) -> Option<Val> {
        let Place::Component(w, uid, _) = self.key(p) else {
            return None;
        };
        let t = self.transforms.get(w).copied().flatten()?;
        let comp = self.p.components.get(w)?.iter().find(|c| c.uid == uid)?;
        let value = |prop: u32| {
            self.p
                .place_ids
                .get(&Place::Component(w, uid, prop))
                .and_then(|&q| self.reg[q as usize].clone())
                .or_else(|| comp.get(prop).map(Val::of))
        };
        let input = value(POSITIONING_INPUT)
            .as_ref()
            .and_then(floats)
            .unwrap_or(Floats { len: 3, f: [0.; 4] });
        let [x, y] = [input.f[0], input.f[1]];
        let [a, b, c, d, tx, ty] = t;
        let (x, y) = match value(POSITIONING_CONVERSION) {
            Some(Val::U32(LOCAL_TO_GLOBAL)) => (a * x + c * y + tx, b * x + d * y + ty),
            Some(Val::U32(GLOBAL_TO_LOCAL)) => {
                let det = a * d - b * c;
                if det.abs() < 1e-12 {
                    return None;
                }
                let (x, y) = (x - tx, y - ty);
                ((d * x - c * y) / det, (a * y - b * x) / det)
            }
            _ => (x, y),
        };
        let mut f = input.f;
        f[0] = x;
        f[1] = y;
        Some(Val::floats(&f[..input.len.max(2)]))
    }

    /// Widget `w`'s stored geometry; the root's is its container
    /// instance's where there is one.
    fn geometry(&self, w: usize) -> &super::Geometry {
        match self.root_geometry {
            Some(g) if w == 0 => g,
            _ => &self.p.geometry[w],
        }
    }

    /// Built-in property `id` of widget `w`, whole.
    fn builtin(&self, w: usize, id: u32) -> Option<Val> {
        let g = self.geometry(w);
        let b = &self.bound[w];
        Some(match id {
            builtin::POSITION => to_bits(b.position.unwrap_or(g.position)),
            builtin::PIVOT => to_bits(b.pivot.unwrap_or(g.pivot)),
            // A bound Size is Scale = Size / intrinsic (the game's Size
            // setter writes Scale): patterns read it into UVScale to repeat
            // at the texture's density. The laid-out size is intrinsic × the
            // unbound Scale.
            builtin::SCALE => {
                let mut s = b.scale.unwrap_or(g.scale);
                if b.scale.is_none()
                    && let (Some(size), Some(laid)) = (b.size, self.sizes.get(w).copied().flatten())
                {
                    for a in 0..2 {
                        if laid[a] > 0. {
                            s[a] = size[a] * g.scale[a] / laid[a];
                        }
                    }
                }
                to_bits(s)
            }
            builtin::ROTATION => Val::f32(b.rotation.unwrap_or(g.rotation)),
            builtin::ALPHA => Val::f32(b.alpha.unwrap_or(g.alpha)),
            builtin::VISIBLE => Val::Bool(u8::from(b.visible.unwrap_or(g.visible))),
            builtin::COLOR => Val::floats(&b.tint.or(g.tint)?.map(f32::from)),
            builtin::SIZE | builtin::DISPLAYED_SIZE => {
                Val::floats(&b.size.or(self.sizes.get(w).copied().flatten())?)
            }
            _ => return None,
        })
    }

    /// Writes `value` through slot access `a`.
    fn store(&mut self, a: &Access, value: &Val) {
        match a {
            Access::Nested(slot) => {
                if let Some(Slot::Nested {
                    widget,
                    inner,
                    path,
                }) = self.p.table.slots.get(*slot)
                {
                    self.nested[*widget].push((inner.clone(), path.clone(), value.dv()));
                }
            }
            Access::Template {
                place,
                field: field @ 2..=5,
                default,
                ..
            } => {
                let base = self.template_value(*place, default);
                self.reg[*place as usize] = Some(set_axis(base, *field, value));
            }
            Access::Template { place, .. } => self.reg[*place as usize] = Some(value.clone()),
            Access::Builtin { w, id, field } => {
                if *id == builtin::TEXT || *id == builtin::IMAGE {
                    if let Val::Str(t) = value {
                        self.text[*w] = Some(t.clone());
                    }
                    return;
                }
                if *id == builtin::LOCALIZATION_ID {
                    if let Val::Str(t) = value {
                        self.loc[*w] = Some(t.clone());
                    }
                    return;
                }
                // One axis of Size keeps the other at the laid-out size.
                if *id == builtin::SIZE && self.bound[*w].size.is_none() {
                    self.bound[*w].size = self.sizes.get(*w).copied().flatten();
                }
                let g = match self.root_geometry {
                    Some(g) if *w == 0 => g,
                    _ => &self.p.geometry[*w],
                };
                write(&mut self.bound[*w], g, *id, *field, value)
            }
            // PropertyField 2/3 write one axis of a vector input.
            Access::Input {
                place,
                field: field @ 2..=5,
                ..
            } => {
                let p = *place as usize;
                let base = self.reg[p]
                    .clone()
                    .or_else(|| self.axis_default.get(p).cloned().flatten());
                self.reg[p] = Some(set_axis(base, *field, value));
            }
            Access::Input { place, .. } => self.reg[*place as usize] = Some(value.clone()),
            // PropertyField 2/3 on the property write one axis of a vector
            // component property (the access button's pivot ratio x),
            // keeping the rest as stored.
            Access::Component {
                place,
                field: field @ 2..=5,
                own,
                found,
                data,
                ..
            } => {
                let p = *place as usize;
                let base = self.reg[p].clone().or_else(|| own.clone());
                self.reg[p] = Some(set_axis(base, *field, value));
                self.nested_field(p, *found, *data, value);
                self.value_component(p);
            }
            Access::Component {
                place, found, data, ..
            } => {
                self.reg[*place as usize] = Some(value.clone());
                self.nested_field(*place as usize, *found, *data, value);
                self.value_component(*place as usize);
            }
            Access::Field { w, data } => {
                let d = *data as usize;
                self.data[d] = Some(Some(value.clone()));
                self.field_write(*w, d, value);
            }
            Access::None | Access::Const(_) | Access::Data(_) => {}
        }
    }

    /// A ValueComponent's MinValue and MaxValue take in its MyValue (see
    /// `VALUE_COMPONENT`); `converts` re-runs their readers. The native
    /// side spans every instance (the hippodrome's tabs share their
    /// biggest size); with one instance here the stored value stands in
    /// for the others.
    fn value_component(&mut self, p: usize) {
        let Some(my) = self.reg[p].as_ref().and_then(floats) else {
            return;
        };
        let outs: Vec<(u32, bool, Option<DefaultValue>)> = self
            .p
            .value_outputs
            .iter()
            .filter(|(i, ..)| *i as usize == p)
            .map(|(_, o, max, v)| (*o, *max, v.clone()))
            .collect();
        for (o, max, stored) in outs {
            let mut v = my;
            if let Some(s) = stored.map(|v| Val::of(&v)).as_ref().and_then(floats) {
                // Lanes past `len` are zero (see `floats`).
                v.len = v.len.max(s.len);
                for k in 0..v.len {
                    v.f[k] = if max {
                        v.f[k].max(s.f[k])
                    } else {
                        v.f[k].min(s.f[k])
                    };
                }
            }
            self.reg[o as usize] = Some(Val::floats(&v));
        }
    }

    /// On a nested container, two steps that name no component are a field
    /// of its data-object input (`Text.-Layout.TextPlane` x from the
    /// quest objective's width): the template reads it as a field write.
    fn nested_field(&mut self, place: usize, found: bool, data: u32, value: &Val) {
        if !found
            && data != u32::MAX
            && let Place::Component(w, ..) = self.key(place)
            && w != 0
        {
            self.field_write(w, data as usize, value);
        }
    }

    /// Passes the fields of a data object (paths from the object's own
    /// step) on to the containers among `targets`, into the input each
    /// target names: once per field, above the stored ones and below any
    /// binding's own write.
    fn forward(
        &mut self,
        targets: &[usize],
        slots: &[Slot],
        fields: &[(Vec<(u32, u32)>, DefaultValue)],
    ) {
        for &t in targets {
            let Some(Slot::Widget {
                widget: Some(w),
                path,
            }) = slots.get(t)
            else {
                continue;
            };
            let [(field, uid)] = path[..] else {
                continue;
            };
            for (p, v) in fields {
                let mut to = vec![(field, uid)];
                to.extend_from_slice(&p[1..]);
                let at = self.nested_base[*w];
                let n = &mut self.nested[*w];
                if let Some(e) = n[..at]
                    .iter_mut()
                    .find(|(i, q, _)| i.is_empty() && *q == to)
                {
                    e.2 = v.clone();
                    continue;
                }
                n.insert(at, (Vec::new(), to, v.clone()));
                self.nested_base[*w] = at + 1;
            }
        }
    }

    /// Passes a write to a field of container `w`'s data-object input (path
    /// `paths[d]`) on to its template.
    fn field_write(&mut self, w: usize, d: usize, value: &Val) {
        let mut path = self.p.paths[d].clone();
        // One axis of a vector field lands in the whole value the instance
        // stores or a binding wrote, as the template reads it.
        let mut value = value.dv();
        if let Some(last) = path.last_mut()
            && last.0 >= 2
        {
            let field = last.0;
            last.0 = 1;
            let base = self.nested[w]
                .iter()
                .rev()
                .find(|(inner, p, _)| inner.is_empty() && *p == path)
                .map(|(_, _, v)| Val::of(v));
            value = set_axis(base, field, &Val::of(&value)).dv();
        }
        self.nested[w].push((Vec::new(), path, value));
    }

    /// The text a label or image slot shows, for last-writer-wins on it.
    fn text_at(&self, a: &Access) -> Option<Arc<str>> {
        match a {
            Access::Builtin { w, id, .. } if *id == builtin::TEXT || *id == builtin::IMAGE => {
                self.text.get(*w).cloned().flatten()
            }
            _ => None,
        }
    }

    /// Stores `value` through slot `t`: true when its place now reads
    /// differently (label and image text: bindings never read it back, but
    /// its writers still need last-writer-wins).
    fn put(
        &mut self,
        t: usize,
        value: &Val,
        component_default: &mut dyn FnMut(Guid, u32) -> Option<DefaultValue>,
        game_data: &mut GameData,
    ) -> bool {
        let p = self.p;
        let a = p.access.get(t).unwrap_or(&Access::None);
        let placed = p.places.get(t).copied().flatten().is_some();
        // One axis of a field of the template's own object property that
        // nothing wrote yet keeps the others as the game data has them.
        if let Access::Component {
            place: q,
            field: 2..=5,
            found: false,
            data,
            ..
        } = *a
            && data != u32::MAX
            && let Place::Component(0, object, _) = self.key(q as usize)
            && self.props.iter().any(|p| p.uid == object)
            && self.reg[q as usize].is_none()
        {
            let mut whole = p.paths[data as usize].clone();
            if let Some(last) = whole.last_mut() {
                last.0 = 1;
            }
            self.reg[q as usize] = game_data(&whole).map(|v| Val::of(&v));
        }
        let text_before = self.text_at(a);
        let before = if placed {
            self.read(a, component_default, game_data)
        } else {
            None
        };
        self.store(a, value);
        placed
            && (before != self.read(a, component_default, game_data)
                || text_before != self.text_at(a))
    }
}

/// Evaluates the plain bindings of `asset` over its widgets' stored values.
/// `component_default` gives a component template's default for a custom
/// property. Returns what landed on each widget.
pub fn evaluate(
    asset: &Asset,
    widgets: &[Widget],
    feed: &Feed,
    component_default: &mut dyn FnMut(Guid, u32) -> Option<DefaultValue>,
    game_data: &mut GameData,
    enum_value: &mut dyn FnMut(Guid, u32, u32) -> Option<DefaultValue>,
) -> Evaluated {
    let Feed {
        inputs,
        sizes,
        animated,
        settled,
        inherited,
        outputs,
        sampling,
        root_components,
        instance_components,
        prepared,
        ..
    } = *feed;
    let read;
    let prepared = match prepared {
        Some(p) => p,
        None => {
            read = Prepared::new(asset, widgets);
            &read
        }
    };
    let table = &prepared.table;
    if root_components.is_empty()
        && table.bindings.is_empty()
        && animated.is_empty()
        && settled.is_empty()
        && inherited.is_empty()
    {
        return Evaluated {
            bound: vec![Bound::default(); widgets.len()],
            text: vec![None; widgets.len()],
            loc: vec![None; widgets.len()],
            inputs: vec![HashMap::new(); widgets.len()],
            runtime: vec![Vec::new(); widgets.len()],
            nested: vec![Vec::new(); widgets.len()],
            template: Arc::default(),
            components: HashMap::new(),
            trace: Arc::default(),
        };
    }
    let props = asset.properties();
    let (read_default, axis_default) = prepared
        .container_defaults
        .get_or_init(|| container_defaults(prepared, component_default));
    let places = prepared.keys.len();
    let mut ev = Eval {
        p: prepared,
        sizes,
        transforms: feed.transforms,
        root_geometry: feed.root_geometry,
        bound: vec![Bound::default(); widgets.len()],
        text: vec![None; widgets.len()],
        loc: vec![None; widgets.len()],
        reg: vec![None; places],
        extra: Vec::new(),
        inputs,
        outputs,
        props,
        fed: vec![None; places],
        out: vec![None; places],
        read_default,
        axis_default,
        component_default: vec![None; places],
        data: vec![None; prepared.paths.len()],
        nested: vec![Vec::new(); widgets.len()],
        root_fields: Vec::new(),
        nested_base: vec![0; widgets.len()],
    };
    if let Some(root) = prepared.components.first() {
        for c in root {
            let Some(t) = c.template else {
                continue;
            };
            for (tpl, prop, v) in root_components {
                if *tpl == t {
                    let p = ev.place(Place::Component(0, c.uid, *prop));
                    ev.reg[p as usize] = Some(Val::of(v));
                }
            }
        }
        // A component the root's WidgetType brings (the radial list's own
        // sector info, read as `SecorAngle`) is not declared here: its
        // reads take the value by property.
        for (tpl, prop, v) in root_components {
            if root.iter().any(|c| c.template == Some(*tpl)) {
                continue;
            }
            let reads: Vec<u32> = prepared
                .place_ids
                .iter()
                .filter(|(k, _)| matches!(k, Place::Component(0, _, p) if p == prop))
                .map(|(_, &id)| id)
                .collect();
            for p in reads {
                ev.reg[p as usize] = Some(Val::of(v));
            }
        }
    }
    if !widgets.is_empty() {
        for (uid, prop, v) in instance_components {
            let p = ev.place(Place::Component(0, *uid, *prop));
            ev.reg[p as usize] = Some(Val::of(v));
        }
    }
    // What a parent template's bindings write into this file's widgets: the
    // widget itself, or a container to pass the rest to.
    let mut inherited_writes: Vec<(Access, Val)> = Vec::new();
    for (inner, path, value) in inherited {
        // A parent's write into a field of this template's data input: what
        // the root reads along that path.
        let Some((first, rest)) = inner.split_first() else {
            // Later writes (a binding after the stored value) win.
            ev.root_fields.retain(|(p, _)| p != path);
            ev.root_fields.push((path.clone(), value.clone()));
            if let [(_, object), (field, uid)] = path[..]
                && props.iter().any(|p| p.uid == object)
                && let Some(&q) = prepared.place_ids.get(&Place::Component(0, object, uid))
            {
                let q = q as usize;
                ev.reg[q] = Some(match field {
                    2..=5 => set_axis(ev.reg[q].clone(), field, &Val::of(value)),
                    _ => Val::of(value),
                });
            }
            if let Some(d) = prepared
                .paths
                .iter()
                .zip(&prepared.path_owner)
                .position(|(p, &o)| o == 0 && p == path)
            {
                ev.data[d] = Some(Some(Val::of(value)));
            }
            continue;
        };
        let Some(&widget) = prepared.ids.get(first) else {
            continue;
        };
        if !rest.is_empty() {
            ev.nested[widget].push((rest.to_vec(), path.clone(), value.clone()));
            continue;
        }
        let slot = Slot::Widget {
            widget: Some(widget),
            path: path.clone(),
        };
        let place = place_of(&slot).map(|k| ev.place(k));
        let a = widget_access(widget, path, place, &prepared.components, widgets, u32::MAX);
        inherited_writes.push((a, Val::of(value)));
    }
    for (w, path, value) in &prepared.stored_fields {
        if let Some(n) = ev.nested.get_mut(*w) {
            n.push((Vec::new(), path.clone(), value.clone()));
            ev.nested_base[*w] = n.len();
        }
    }
    for (a, v) in &inherited_writes {
        ev.store(a, v);
    }
    let animated: Vec<(usize, Val)> = animated.iter().map(|(s, v)| (*s, Val::of(v))).collect();
    // A clip writes its tracks every frame: before the bindings, so they
    // carry animated sources on, and again after, so a directly animated
    // property shows the clip's value.
    for (slot, v) in settled {
        ev.store(
            prepared.access.get(*slot).unwrap_or(&Access::None),
            &Val::of(v),
        );
    }
    // Bindings tagged with one of this template's widget styles run only in
    // that style. Other tags (a text atom's font-size rule) do not filter.
    let styles = &prepared.styles;
    // As `current_style`, from the prepared geometry.
    let style = match inputs.get(&CURRENT_ENUM_VALUE) {
        Some(DefaultValue::U32(v)) => *v,
        Some(DefaultValue::S32(v)) => *v as u32,
        Some(_) => 0,
        None => styles.first().copied().unwrap_or(0),
    };
    let mut runtime: Vec<Vec<GameFed>> = vec![Vec::new(); widgets.len()];
    // Per binding: what it read and wrote on its last run, the arguments
    // at `Prepared::arg_at`.
    let mut trace: Vec<Option<(u32, u8, Option<Val>)>> = vec![None; table.bindings.len()];
    let mut trace_args: Vec<Val> =
        vec![Val::None; prepared.arg_at.last().copied().unwrap_or(0) as usize];
    let mut read_args: Vec<Option<Val>> = Vec::new();
    let mut args: Vec<Val> = Vec::new();
    let empty: Arc<str> = Arc::from("");
    // Bindings run in file order and feed each other; repeat until nothing
    // changes (the band's corners: ceiling → mask size → half width →
    // Sign → position is five deep).
    // A worklist over sweeps: after the first, a binding reruns only when a
    // place it reads changed since it last ran (a binding is a function of
    // its sources), and a sweep that changes nothing ends it. One stamp
    // orders every run and change across sweeps.
    let mut changed_at: Vec<u32> = vec![0; places];
    let mut ran_at: Vec<u32> = vec![0; table.bindings.len()];
    let mut stamp: u32 = 1;
    let mut changed;
    // Several bindings may write one place, and the last in file order
    // wins each sweep: when one changes it, the later writers run again
    // this sweep (a bitset over bindings).
    let mut forced: Vec<u64> = vec![0; table.bindings.len().div_ceil(64)];
    let mut writer: Option<usize>;
    let full_sweeps = FULL_SWEEPS.with(|c| c.get());
    // Groups (see `Prepared::groups`) a member changed something in this
    // sweep: all their members run again, in order, the next one.
    let mut regroup: Vec<u64> = vec![0; prepared.groups.len().div_ceil(64)];
    // Stores `$v` through slot `$t`, noting whether its place's value moved.
    macro_rules! put {
        ($t:expr, $v:expr) => {{
            let t: usize = $t;
            if ev.put(t, $v, component_default, game_data) {
                let p = prepared.places[t].expect("a changed slot has a place") as usize;
                changed_at[p] = stamp;
                for &(i, o) in &prepared.converts {
                    if i as usize == p {
                        changed_at[o as usize] = stamp;
                    }
                }
                changed = true;
                for &w in &prepared.writers[p] {
                    if writer.is_none_or(|bi| w as usize > bi) {
                        forced[w as usize / 64] |= 1 << (w % 64);
                    }
                }
                if let Some(bi) = writer {
                    for &g in &prepared.group_of[bi] {
                        regroup[g as usize / 64] |= 1 << (g % 64);
                    }
                }
            }
        }};
    }
    for pass in 0..MAX_PASSES {
        changed = false;
        writer = None;
        for (slot, v) in &animated {
            put!(*slot, v);
        }
        if pass > 0 && table.bindings.is_empty() {
            break;
        }
        for (bi, b) in table.bindings.iter().enumerate() {
            let stale = |s: &usize| {
                prepared
                    .places
                    .get(*s)
                    .copied()
                    .flatten()
                    .is_some_and(|p| changed_at[p as usize] > ran_at[bi])
            };
            let force = forced[bi / 64] & (1 << (bi % 64)) != 0;
            if pass > 0 && !force && !full_sweeps && !b.sources.iter().any(stale) {
                continue;
            }
            forced[bi / 64] &= !(1 << (bi % 64));
            writer = Some(bi);
            ran_at[bi] = stamp;
            stamp += 1;
            let Some(conv) = prepared.converters[bi] else {
                continue;
            };
            // Mode 1 is two-way (its converter `bba41a31` only appears there):
            // it syncs source to target first, then writes edits back.
            if b.mode > 1
                || b.sources.is_empty()
                || (b.entry != 0 && b.entry != style && styles.contains(&b.entry))
            {
                continue;
            }
            let at = prepared.arg_at[bi] as usize;
            if matches!(conv, Converter::EventToTrue | Converter::EventToFalse) {
                let shown = matches!(
                    b.sources.first().and_then(|&s| table.slots.get(s)),
                    Some(Slot::Widget {
                        widget: Some(0) | None,
                        path,
                    }) if path[..] == [(1, BUILTIN | builtin::SHOWN)]
                );
                if !shown {
                    continue;
                }
                let v = Val::Bool(u8::from(conv == Converter::EventToTrue));
                trace_args[at] = Val::None;
                trace[bi] = Some((at as u32, 1, Some(v.clone())));
                for &t in &b.targets {
                    put!(t, &v);
                }
                continue;
            }
            // EventAssign needs only its value; the event is often a
            // dispatcher's runtime action that nothing here can read.
            let sources = if conv == Converter::EventAssign {
                &b.sources[..1]
            } else {
                &b.sources[..]
            };
            read_args.clear();
            for &s in sources {
                let a = prepared.access.get(s).unwrap_or(&Access::None);
                let v = ev
                    .read(a, component_default, game_data)
                    // An object property with nothing in it (`-Data` the
                    // game fills) is game data passing through.
                    .filter(|v| !(matches!(a, Access::Template { .. }) && *v == Val::None));
                read_args.push(v);
            }
            args.clear();
            // A container's object output passed on whole (the object menu's
            // background into its controller): the game passes the object
            // itself, so its fields go along (see `field_key`), and what the
            // receiving template writes into it (the controller sets the
            // background's height) comes back. It reads as no value, so
            // this does not wait on it resolving.
            if conv == Converter::Assign
                && let [s] = sources[..]
                && let Some(Slot::Widget {
                    widget: Some(w),
                    path,
                }) = table.slots.get(s)
                && let [(_, object)] = path[..]
            {
                let fields = |w: usize, object: u32| -> Vec<(Vec<(u32, u32)>, DefaultValue)> {
                    let Some(out) = ev.outputs.get(w).copied().flatten() else {
                        return Vec::new();
                    };
                    let Some(DefaultValue::Vec(list)) = out.get(&field_key(object, 0)) else {
                        return Vec::new();
                    };
                    list.iter()
                        .filter_map(|&f| {
                            Some((
                                vec![(1, object), (1, f)],
                                out.get(&field_key(object, f))?.clone(),
                            ))
                        })
                        .collect()
                };
                let there = fields(*w, object);
                let back: Vec<_> = b
                    .targets
                    .iter()
                    .filter_map(|&t| match table.slots.get(t) {
                        Some(Slot::Widget {
                            widget: Some(to),
                            path,
                        }) if let [(_, input)] = path[..] => Some(fields(*to, input)),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                if !there.is_empty() {
                    ev.forward(&b.targets, &table.slots, &there);
                }
                if !back.is_empty() {
                    ev.forward(&[s], &table.slots, &back);
                }
            }
            if read_args.iter().all(Option::is_some) {
                args.extend(read_args.iter().flatten().cloned());
            } else if sampling
                && matches!(conv, Converter::Equal | Converter::NotEqual)
                && read_args
                    .iter()
                    .any(|v| matches!(v, Some(Val::Str(s)) if s.is_empty()))
                && sources.iter().zip(&read_args).all(|(&s, v)| {
                    v.is_some()
                        // A field of a data object, not a widget's own
                        // property (`Icon · Image` is set by path and not
                        // read back here). A nested container's field this
                        // file did not write is that template's output
                        // (the popup header's `Header_DisplayedOutput`
                        // text, which its label fills), not game data:
                        // unknown, not empty.
                        || match table.slots.get(s) {
                            Some(Slot::Widget { widget, path }) if path.len() >= 2 => {
                                // Two steps parse as a component path; one
                                // that names no component is a field too.
                                let nested_field = widget.is_some_and(|w| w != 0)
                                    && matches!(
                                        prepared.access.get(s),
                                        Some(
                                            Access::Field { .. }
                                                | Access::Component { found: false, .. }
                                        )
                                    );
                                !nested_field
                            }
                            _ => false,
                        }
                })
            {
                // A game string compared against "" (the archive badges
                // hide on an empty count): game data nothing fills reads as
                // empty, as unset strings do in game.
                args.extend(
                    read_args
                        .iter()
                        .map(|v| v.clone().unwrap_or(Val::Str(empty.clone()))),
                );
            } else if sampling && conv == Converter::EnumToValue {
                // A game-data enum nothing here supplies: with sample data,
                // the rule's first mapped entry (0 asks for it).
                args.push(Val::U32(0));
            } else {
                let from: Vec<(u32, u32)> = b
                    .sources
                    .iter()
                    .find_map(|&s| match table.slots.get(s) {
                        Some(Slot::Widget { widget, path })
                            if widget.is_none() || path.len() >= 2 =>
                        {
                            Some(path.clone())
                        }
                        // A template property passed on: the parent's
                        // context names it.
                        Some(Slot::Template(i, _)) => Some(vec![(1, props.get(*i)?.uid)]),
                        _ => None,
                    })
                    .unwrap_or_default();
                for &t in &b.targets {
                    if let Some(Slot::Widget {
                        widget: Some(w),
                        path,
                    }) = table.slots.get(t)
                        && let [(_, uid)] = path[..]
                        && uid & BUILTIN == 0
                        && !runtime[*w].iter().any(|r| r.0 == uid)
                    {
                        runtime[*w].push((uid, from.clone()));
                    }
                }
                // A data-object input passed on whole (`-Header` into the
                // popup header): its fields go along to the container.
                if conv == Converter::Assign
                    && let [s] = sources[..]
                    && let Some(Slot::Template(i, _)) = table.slots.get(s)
                    && let Some(src) = props.get(*i).map(|p| p.uid)
                {
                    let fields: Vec<_> = ev
                        .root_fields
                        .iter()
                        .filter(|(p, _)| p.len() >= 2 && p[0].1 == src)
                        .cloned()
                        .collect();
                    ev.forward(&b.targets, &table.slots, &fields);
                }
                continue;
            }
            trace_args[at..at + args.len()].clone_from_slice(&args);
            trace[bi] = Some((at as u32, args.len() as u8, None));
            if conv == Converter::EnumToValue {
                let e = match args[0] {
                    Val::U32(v) => v,
                    Val::S32(v) => v as u32,
                    _ => continue,
                };
                let Some(value) = b
                    .enum_rule
                    .and_then(|(a, rule)| enum_value(a, rule, e))
                    .map(|v| Val::of(&v))
                else {
                    continue;
                };
                if let Some(t) = &mut trace[bi] {
                    t.2 = Some(value.clone());
                }
                for &t in &b.targets {
                    put!(t, &value);
                }
                continue;
            }
            if conv == Converter::EnumToBoolean {
                // Entry ids are ~1.4e9: compare exactly, not as floats.
                let e = match args[0] {
                    Val::U32(v) => v,
                    Val::S32(v) => v as u32,
                    _ => continue,
                };
                for (k, &t) in b.targets.iter().enumerate() {
                    let on = b
                        .rules
                        .iter()
                        .any(|&(entry, target)| target == k && entry == e);
                    let v = Val::Bool(u8::from(on));
                    if let Some(t) = trace[bi].as_mut().filter(|t| t.2.is_none()) {
                        t.2 = Some(v.clone());
                    }
                    put!(t, &v);
                }
                continue;
            }
            let Some(value) = convert_val(conv, &args) else {
                continue;
            };
            if let Some(t) = &mut trace[bi] {
                t.2 = Some(value.clone());
            }
            for &t in &b.targets {
                put!(t, &value);
            }
        }
        if !changed {
            break;
        }
        for (g, members) in prepared.groups.iter().enumerate() {
            if regroup[g / 64] & (1 << (g % 64)) != 0 {
                for &m in members {
                    forced[m as usize / 64] |= 1 << (m % 64);
                }
            }
        }
        regroup.fill(0);
    }
    for (slot, v) in &animated {
        ev.store(prepared.access.get(*slot).unwrap_or(&Access::None), v);
    }
    // The parent's writes win over this file's own bindings. Writes to
    // pass further down were queued above already.
    for (a, v) in &inherited_writes {
        ev.store(a, v);
    }
    let mut child_inputs: Vec<HashMap<u32, DefaultValue>> = vec![HashMap::new(); widgets.len()];
    let mut template: HashMap<u32, DefaultValue> = HashMap::new();
    let mut components: HashMap<(usize, u32, u32), DefaultValue> = HashMap::new();
    for (p, v) in ev.reg.iter().enumerate() {
        let Some(v) = v else {
            continue;
        };
        match ev.key(p) {
            Place::Widget(w, uid) => {
                child_inputs[w].insert(uid, v.dv());
            }
            Place::Component(w, comp, prop) => {
                // Fields of the template's own object property are outputs
                // too: a parent passes the object on whole (the object
                // menu's background size, which sets the controller's pivot).
                if w == 0 && props.iter().any(|p| p.uid == comp) {
                    template.insert(field_key(comp, prop), v.dv());
                    let fields = template
                        .entry(field_key(comp, 0))
                        .or_insert(DefaultValue::Vec(Vec::new()));
                    if let DefaultValue::Vec(f) = fields {
                        f.push(prop);
                    }
                }
                components.insert((w, comp, prop), v.dv());
            }
            Place::Template(i) => {
                if let Some(p) = props.get(i) {
                    template.insert(p.uid, v.dv());
                }
            }
        }
    }
    // A later pass may have resolved what an earlier one could not.
    for (r, i) in runtime.iter_mut().zip(&child_inputs) {
        r.retain(|(uid, _)| !i.contains_key(uid));
    }
    // A field write from game data (`Text.-Data.ID ← _Text.TextData.ID`)
    // makes the input stand for the object the field is read from: the
    // input is fed from that path, which names it for sample data inside
    // (the atom's `-Data.Text` is the objective's text, not a figure).
    for b in &table.bindings {
        let Some(from) = b.sources.iter().find_map(|&s| match table.slots.get(s) {
            Some(Slot::Widget { widget, path }) if widget.is_none() || path.len() >= 2 => {
                Some(path)
            }
            _ => None,
        }) else {
            continue;
        };
        for &t in &b.targets {
            if let Some(Slot::Widget {
                widget: Some(w),
                path,
            }) = table.slots.get(t)
                && let [(_, uid), (_, field)] = path[..]
                && uid & BUILTIN == 0
                && from.last().is_some_and(|l| l.1 == field)
                && !runtime[*w].iter().any(|r| r.0 == uid)
                && !child_inputs[*w].contains_key(&uid)
            {
                runtime[*w].push((uid, from[..from.len() - 1].to_vec()));
            }
        }
    }
    Evaluated {
        bound: ev.bound,
        text: ev
            .text
            .into_iter()
            .map(|t| t.map(|t| t.to_string()))
            .collect(),
        loc: ev
            .loc
            .into_iter()
            .map(|t| t.map(|t| t.to_string()))
            .collect(),
        inputs: child_inputs,
        runtime,
        nested: ev.nested,
        template: Arc::new(template),
        components,
        trace: Arc::new(Traces {
            args: trace_args,
            runs: trace,
        }),
    }
}

/// Only the `animated` slot writes, no bindings: what a clip does to the
/// stored values.
pub fn evaluate_writes(
    asset: &Asset,
    widgets: &[Widget],
    animated: &[(usize, DefaultValue)],
) -> Evaluated {
    let table = asset.bindings(widgets);
    let mut bound = vec![Bound::default(); widgets.len()];
    for (slot, v) in animated {
        if let Some(Slot::Widget {
            widget: Some(w),
            path,
        }) = table.slots.get(*slot)
            && let [(field, id)] = path[..]
            && id & BUILTIN != 0
        {
            let g = asset.geometry(&widgets[*w]).unwrap_or_default();
            write(&mut bound[*w], &g, id & !BUILTIN, field, &Val::of(v));
        }
    }
    Evaluated {
        text: vec![None; bound.len()],
        loc: vec![None; bound.len()],
        inputs: vec![HashMap::new(); bound.len()],
        runtime: vec![Vec::new(); bound.len()],
        nested: vec![Vec::new(); bound.len()],
        template: Arc::default(),
        components: HashMap::new(),
        trace: Arc::default(),
        bound,
    }
}

fn truthy(v: &Val) -> bool {
    match v {
        Val::Bool(b) => *b != 0,
        Val::Str(s) => !s.is_empty(),
        other => floats(other).is_some_and(|f| f.iter().any(|x| *x != 0.0)),
    }
}

fn text_of(v: &Val) -> String {
    match v {
        Val::Str(s) => s.to_string(),
        Val::Bool(b) => (*b != 0).to_string(),
        Val::S32(i) => i.to_string(),
        Val::U32(u) => u.to_string(),
        other => floats(other).map_or_else(String::new, |f| {
            f.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }),
    }
}

/// Applies a converter to its source values; None when the shapes do not
/// fit (the target then keeps what it had).
pub fn convert(c: Converter, args: &[DefaultValue]) -> Option<DefaultValue> {
    let args: Vec<Val> = args.iter().map(Val::of).collect();
    convert_val(c, &args).map(|v| v.dv())
}

fn convert_val(c: Converter, args: &[Val]) -> Option<Val> {
    use Converter::*;
    let boolean = |b: bool| Some(Val::Bool(u8::from(b)));
    let first = args.first()?;
    let shaped = |out: &Floats| {
        if out.len == 1 {
            Val::f32(out.f[0])
        } else {
            Val::floats(out)
        }
    };
    // Numbers per component, scalars broadcast; the widest operand sets
    // the shape of the result.
    let numeric = |f: &dyn Fn(f32, f32) -> f32| -> Option<Val> {
        let mut n = 0;
        for a in args {
            n = n.max(floats(a)?.len);
        }
        let at = |v: &Floats, i: usize| {
            if v.len == 1 {
                v.f[0]
            } else if i < v.len {
                v.f[i]
            } else {
                0.0
            }
        };
        let mut out = Floats { len: n, f: [0.; 4] };
        let first = floats(first)?;
        for i in 0..n {
            out.f[i] = args[1..].iter().fold(at(&first, i), |acc, v| {
                f(acc, at(&floats(v).expect("checked above"), i))
            });
        }
        Some(shaped(&out))
    };
    // Per component of the first argument (`numeric` folds over the others,
    // so with one argument it would never call `f`).
    let unary = |f: &dyn Fn(f32) -> f32| -> Option<Val> {
        let mut v = floats(first)?;
        for x in v.iter_mut() {
            *x = f(*x);
        }
        Some(shaped(&v))
    };
    let scalar = |v: &Val| floats(v).and_then(|f| f.first().copied());
    match c {
        Assign | NumberToFloat => match (c, first) {
            (NumberToFloat, v) => scalar(v).map(Val::f32),
            _ => Some(first.clone()),
        },
        BoolToNumber => Some(Val::f32(f32::from(u8::from(truthy(first))))),
        Add => numeric(&|a, b| a + b),
        Subtract => numeric(&|a, b| a - b),
        Multiply => numeric(&|a, b| a * b),
        Divide => numeric(&|a, b| if b == 0.0 { 0.0 } else { a / b }),
        Modulo => numeric(&|a, b| if b == 0.0 { 0.0 } else { a % b }),
        Min => numeric(&f32::min),
        Max => numeric(&f32::max),
        Clamp => {
            let (lo, hi) = (scalar(args.get(1)?)?, scalar(args.get(2)?)?);
            unary(&|x| x.clamp(lo.min(hi), hi.max(lo)))
        }
        Floor => unary(&f32::floor),
        Ceil => unary(&f32::ceil),
        Round => unary(&f32::round),
        Abs => unary(&f32::abs),
        Sign => unary(&|x| -x),
        Sin => unary(&f32::sin),
        Cos => unary(&f32::cos),
        DegreeToRadian => unary(&f32::to_radians),
        SquareRoot => unary(&|x| x.max(0.0).sqrt()),
        Equal | NotEqual => {
            let b = args.get(1)?;
            let same = match (first, b) {
                (Val::Str(x), Val::Str(y)) => x == y,
                _ => match (floats(first), floats(b)) {
                    (Some(x), Some(y)) => x[..] == y[..],
                    _ => first == b,
                },
            };
            boolean(same == (c == Equal))
        }
        LessThan => boolean(scalar(first)? < scalar(args.get(1)?)?),
        LessOrEqual => boolean(scalar(first)? <= scalar(args.get(1)?)?),
        IsInRangeButNotEqual => {
            let x = scalar(first)?;
            boolean(x > scalar(args.get(1)?)? && x < scalar(args.get(2)?)?)
        }
        Greater => boolean(scalar(first)? > scalar(args.get(1)?)?),
        GreaterOrEqual => boolean(scalar(first)? >= scalar(args.get(1)?)?),
        IsInRange => {
            let x = scalar(first)?;
            boolean(x >= scalar(args.get(1)?)? && x <= scalar(args.get(2)?)?)
        }
        LogicalAnd => boolean(args.iter().all(truthy)),
        LogicalInvert => boolean(!truthy(first)),
        IsEmpty => boolean(match first {
            Val::None => true,
            Val::Str(s) => s.is_empty(),
            _ => false,
        }),
        NumberToBool => boolean(truthy(first)),
        IfTrueAssign => truthy(args.get(1)?).then(|| first.clone()),
        EventAssign => Some(first.clone()),
        EventToTrue => Some(Val::Bool(1)),
        EventToFalse => Some(Val::Bool(0)),
        ValueToString => Some(Val::Str(text_of(first).into())),
        LogicalOr => boolean(args.iter().any(truthy)),
        LogicalXor => boolean(args.iter().filter(|v| truthy(v)).count() % 2 == 1),
        IfTrueFalseAssign => {
            let pick = if truthy(args.get(2)?) {
                first
            } else {
                args.get(1)?
            };
            Some(pick.clone())
        }
        IfFalseAssign => (!truthy(args.get(1)?)).then(|| first.clone()),
        ConcatValues => Some(Val::Str(
            args.iter().map(text_of).collect::<String>().into(),
        )),
        // Per target; `evaluate` applies the rules.
        EnumToBoolean | EnumToValue => None,
    }
}

// PropertyField 2 and 3 address the x and y of a vector property (1 is
// the whole property).
fn write(b: &mut Bound, g: &super::Geometry, id: u32, field: u32, v: &Val) {
    let axis = match field {
        2..=5 => Some(field as usize - 2),
        _ => None,
    };
    let vector = |cur: [f32; 3]| -> Option<[f32; 3]> {
        match axis {
            // A vector into one axis gives that axis its own component
            // (`Size.y = -Size - 5` keeps the height).
            Some(a) => {
                let f = floats(v)?;
                let x = f.get(a).or(f.first()).copied()?;
                let mut c = cur;
                *c.get_mut(a)? = x;
                Some(c)
            }
            None => vec3(v, cur[2]),
        }
    };
    let scalar = || floats(v).and_then(|f| f.first().copied());
    match id {
        builtin::POSITION => b.position = vector(b.position.unwrap_or(g.position)).or(b.position),
        builtin::PIVOT => b.pivot = vector(b.pivot.unwrap_or(g.pivot)).or(b.pivot),
        builtin::SCALE => b.scale = vector(b.scale.unwrap_or(g.scale)).or(b.scale),
        builtin::SIZE => {
            let cur = b.size.map_or([0.; 3], |s| [s[0], s[1], 0.]);
            if let Some(s) = vector(cur) {
                b.size = Some([s[0], s[1]]);
            }
        }
        builtin::ROTATION => b.rotation = scalar().or(b.rotation),
        builtin::ALPHA => b.alpha = scalar().or(b.alpha),
        builtin::COLOR => {
            if let Some(c) = floats(v).filter(|c| c.len() >= 3) {
                b.tint = Some([0, 1, 2].map(|i| c[i].clamp(0., 255.) as u8));
            }
        }
        builtin::VISIBLE => {
            if let Val::Bool(x) = v {
                b.visible = Some(*x != 0);
            }
        }
        builtin::LINE_SPACING => {
            if let Some(x) = floats(v).and_then(|f| f.first().copied()) {
                b.line_spacing = Some(x);
            }
        }
        builtin::UV_OFFSET | builtin::UV_SCALE | builtin::TEXT_PLANE => {
            let slot = match id {
                builtin::UV_OFFSET => &mut b.uv_offset,
                builtin::UV_SCALE => &mut b.uv_scale,
                _ => &mut b.text_plane,
            };
            let Some(f) = floats(v) else { return };
            match axis {
                Some(a) if a < 2 => slot[a] = f.get(a).or(f.first()).copied(),
                Some(_) => {}
                None if f.len() >= 2 => *slot = [Some(f[0]), Some(f[1])],
                None => *slot = [f.first().copied(); 2],
            }
        }
        _ => {
            let label_enum = |ids: [u32; 2]| ids.iter().position(|&i| i == id);
            let value = match v {
                Val::U32(x) => Some(*x),
                Val::S32(x) => u32::try_from(*x).ok(),
                _ => None,
            };
            let slot = if let Some(a) = label_enum(builtin::SIZING) {
                Some((&mut b.sizing, a))
            } else if let Some(a) = label_enum(builtin::ALIGNMENT) {
                Some((&mut b.alignment, a))
            } else {
                label_enum(builtin::OVERFLOW).map(|a| (&mut b.overflow, a))
            };
            // Entry values are 0..5; a large one is an entry's name hash
            // from an enum rule, which this cannot map.
            if let (Some((slot, a)), Some(x)) = (slot, value.filter(|&x| x < 16)) {
                slot[a] = Some(x);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bound, Converter, Val, builtin, convert, pick, set_axis, write};
    use crate::phx::DefaultValue;

    fn f(x: f32) -> DefaultValue {
        DefaultValue::F32(x.to_bits())
    }

    fn v(xs: &[f32]) -> DefaultValue {
        DefaultValue::Vec(xs.iter().map(|x| x.to_bits()).collect())
    }

    #[test]
    fn one_input_converters_apply() {
        // The governors bar mirrors its band's left corner pieces with
        // Sign(half width).
        assert_eq!(convert(Converter::Sign, &[f(602.)]), Some(f(-602.)));
        assert_eq!(convert(Converter::Abs, &[f(-3.)]), Some(f(3.)));
        assert_eq!(
            convert(Converter::Floor, &[v(&[1.5, -1.5])]),
            Some(v(&[1., -2.]))
        );
        assert_eq!(convert(Converter::Ceil, &[f(1.2)]), Some(f(2.)));
    }

    #[test]
    fn arithmetic_broadcasts_scalars_over_vectors() {
        // The commonest shape: Size / 2 → Position, to centre on a pivot.
        assert_eq!(
            convert(Converter::Divide, &[v(&[100., 40.]), f(2.)]),
            Some(v(&[50., 20.]))
        );
        assert_eq!(
            convert(Converter::Subtract, &[f(5.), f(2.), f(1.)]),
            Some(f(2.))
        );
        assert_eq!(
            convert(Converter::Clamp, &[f(1.7), f(0.), f(1.)]),
            Some(f(1.))
        );
        assert_eq!(convert(Converter::Divide, &[f(1.), f(0.)]), Some(f(0.)));
    }

    #[test]
    fn conditionals_pick_or_hold() {
        let on = DefaultValue::Bool(1);
        let off = DefaultValue::Bool(0);
        assert_eq!(
            convert(Converter::IfTrueFalseAssign, &[f(1.), f(0.), on.clone()]),
            Some(f(1.))
        );
        assert_eq!(
            convert(Converter::IfTrueAssign, &[f(3.), off.clone()]),
            None
        );
        assert_eq!(
            convert(Converter::IfFalseAssign, &[f(3.), off.clone()]),
            Some(f(3.))
        );
        assert_eq!(
            convert(Converter::LogicalInvert, std::slice::from_ref(&on)),
            Some(off.clone())
        );
        assert_eq!(
            convert(Converter::LessThan, &[f(1.), f(2.)]),
            Some(on.clone())
        );
        assert_eq!(
            convert(Converter::IsInRangeButNotEqual, &[f(1.), f(0.), f(1.)]),
            Some(off)
        );
    }

    // ConcatValues builds image paths and labels from parts.
    #[test]
    fn strings_join_in_order() {
        let s = |t: &str| DefaultValue::Str(t.into());
        assert_eq!(
            convert(Converter::ConcatValues, &[s("("), f(12.), s(")")]),
            Some(s("(12)"))
        );
        assert_eq!(
            convert(Converter::ValueToString, &[f(1250.)]),
            Some(s("1250"))
        );
    }

    // The governors bar writes its band's Size input x and y in two
    // bindings; the second must not wipe the first.
    #[test]
    fn axis_writes_merge_into_a_vector_input() {
        let (f, v) = (|x: f32| Val::of(&f(x)), |xs: &[f32]| Val::of(&v(xs)));
        let size = set_axis(Some(v(&[1200., 32.])), 2, &f(1332.));
        let size = set_axis(Some(size), 3, &f(35.5));
        assert_eq!(size, v(&[1332., 35.5]));
        // A vector written into one axis gives that axis its component.
        assert_eq!(set_axis(None, 3, &v(&[7., 9.])), v(&[0., 9.]));
    }

    // Needs the game: the shared popup transition addresses its clips'
    // Start through providers that name the AnimationInstance.
    #[test]
    fn clip_providers_resolve_to_clip_slots() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = crate::vfs::Install::open(dir).unwrap();
        let a = crate::phx::Asset::parse(
            install
                .read("data/ui/studio/generated/f778e8bd-5587-4803-839b-b74e8d6d4548")
                .unwrap(),
        )
        .unwrap();
        let t = a.bindings(&a.widgets());
        let names: Vec<String> = t
            .clips_started_on_open()
            .iter()
            .map(|&c| a.clips()[c].name.clone())
            .collect();
        assert_eq!(names, ["ShowAnimation"]);
    }

    // PropertyField 4/5 are z/w: the popup transition's vec3 track feeds
    // opacity from z.
    #[test]
    fn z_and_w_fields_read_and_write_their_axis() {
        let (f, v) = (|x: f32| Val::of(&f(x)), |xs: &[f32]| Val::of(&v(xs)));
        assert_eq!(pick(&[0., 6., 1.], 4), Some(f(1.)));
        assert_eq!(pick(&[0., 6.], 4), None);
        assert_eq!(
            set_axis(Some(v(&[1., 2.])), 5, &f(9.)),
            v(&[1., 2., 0., 9.])
        );
        let mut b = Bound::default();
        let g = Default::default();
        write(&mut b, &g, builtin::POSITION, 4, &f(3.));
        assert_eq!(b.position, Some([0., 0., 3.]));
        write(&mut b, &g, builtin::POSITION, 5, &f(7.));
        assert_eq!(b.position, Some([0., 0., 3.]));
    }

    // Text atoms forward their `-Layout` fields to the label's enums;
    // entry values land per axis, enum-rule name hashes are dropped.
    #[test]
    fn label_layout_enums_write_per_axis() {
        let mut b = Bound::default();
        let g = Default::default();
        write(&mut b, &g, builtin::OVERFLOW[0], 1, &Val::U32(1));
        write(&mut b, &g, builtin::ALIGNMENT[1], 1, &Val::U32(3));
        write(&mut b, &g, builtin::SIZING[0], 1, &Val::U32(1_428_758_250));
        assert_eq!(b.overflow, [Some(1), None]);
        assert_eq!(b.alignment, [None, Some(3)]);
        assert_eq!(b.sizing, [None, None]);
    }

    // The governors bar's left hairline mirrors its texture by binding
    // UVScale.x to −1; y keeps the stored value.
    #[test]
    fn uv_scale_axis_writes_stay_on_their_axis() {
        let (f, v) = (|x: f32| Val::of(&f(x)), |xs: &[f32]| Val::of(&v(xs)));
        let mut b = Bound::default();
        write(&mut b, &Default::default(), builtin::UV_SCALE, 2, &f(-1.));
        assert_eq!(b.uv_scale, [Some(-1.), None]);
        write(
            &mut b,
            &Default::default(),
            builtin::UV_OFFSET,
            1,
            &v(&[3., 4.]),
        );
        assert_eq!(b.uv_offset, [Some(3.), Some(4.)]);
    }
}
